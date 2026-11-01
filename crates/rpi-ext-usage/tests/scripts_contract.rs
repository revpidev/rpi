//! TE53 FR-A–FR-C: provider-script contract tests, offline.
//!
//! Every test drives the real `scripts/*.py` (python3 stdlib) against a
//! loopback stub HTTP server. The scripts use fixed production origins, so
//! the tests enable the test-only loopback override
//! (`RPI_USAGE_TEST_MODE=1` + `RPI_USAGE_TEST_BASE_URL=http://127.0.0.1:PORT`)
//! together with fictional `test-` credentials. All conventional credential
//! variables and the host-injected `RPI_USAGE_OAUTH_*` variables are removed
//! from the child, and `RPI_CODING_AGENT_DIR` points at a temporary directory,
//! so a developer's real HOME/auth.json can never be read.
//!
//! The matrix covers all eight first-party providers: typed envelope
//! (`windows[]`/`ledgers[]`/`balance[]`, status, reset, duration), the
//! byte-stable legacy `displayText`/quota fields, classified failures
//! (missing credential, 401/403, 429, 5xx, non-JSON, shape drift, invalid
//! percent/reset, timeout, oversized body, refused redirect), read-only
//! request assertions and credential non-leakage. python3 is required: a
//! missing interpreter fails the test instead of skipping it.
//!
//! The stub requires a real Python interpreter and loopback networking, so
//! the file stays unix-gated like the host's own usage-framework tests.

#![cfg(unix)]

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

/// Every credential-bearing or test-control variable a script might read.
/// The harness removes all of them before adding its own test values.
const CREDENTIAL_ENVS: [&str; 18] = [
    "DEEPSEEK_API_KEY",
    "ZAI_API_KEY",
    "GLM_API_KEY",
    "ZAI_CODING_CN_API_KEY",
    "MINIMAX_API_KEY",
    "MINIMAX_CN_API_KEY",
    "KIMI_API_KEY",
    "KIMI_CODING_API_KEY",
    "OPENCODE_API_KEY",
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "XAI_API_KEY",
    "RPI_USAGE_OAUTH_ACCESS",
    "RPI_USAGE_OAUTH_ACCOUNT_ID",
    "RPI_USAGE_TEST_MODE",
    "RPI_USAGE_TEST_BASE_URL",
    "RPI_USAGE_NOW_MS",
    "RPI_OFFLINE",
];

const TEST_KEY: &str = "test-key";
const TEST_OAUTH_ACCESS: &str = "test-oauth-access";
const TEST_OAUTH_ACCOUNT: &str = "test-account-id";
const TEST_STORE_ACCESS: &str = "test-store-access";
const TEST_STORE_ACCOUNT: &str = "test-store-account";
const TEST_USER_ID: &str = "test-user-id-123";

/// Fixed fetch-time clock (2027-01-15T08:00:00Z) for every reset/countdown.
const FIXED_NOW_MS: u64 = 1_800_000_000_000;
const RESET_2H5M_MS: u64 = FIXED_NOW_MS + 7_500_000; // + 2h5m
const RESET_3D22H_MS: u64 = FIXED_NOW_MS + 338_400_000; // + 3d22h
const RESET_48M_ISO: &str = "2027-01-15T08:48:00Z";
const RESET_2H5M_ISO: &str = "2027-01-15T10:05:00Z";
const RESET_3D22H_ISO: &str = "2027-01-19T06:00:00Z";

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

#[derive(Clone, Debug)]
enum StubReply {
    Status(u16, String),
    Raw {
        status: u16,
        headers: Vec<(String, String)>,
        body: String,
    },
    Hang,
}

impl StubReply {
    fn ok(body: impl Into<String>) -> Self {
        Self::Status(200, body.into())
    }

    fn redirect() -> Self {
        Self::Raw {
            status: 302,
            headers: vec![(
                "Location".to_owned(),
                "https://evil.invalid/steal".to_owned(),
            )],
            body: String::new(),
        }
    }

    fn oversized() -> Self {
        Self::ok(format!(r#"{{"pad":"{}"}}"#, "x".repeat(300_000)))
    }
}

struct Stub {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

impl Stub {
    fn origin(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn requests(&self) -> Vec<RecordedRequest> {
        self.requests
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
}

/// Serve one fixed reply for every connection (auth-fallback loops included).
fn stub_always(reply: StubReply) -> Stub {
    stub_with(move |_index| reply.clone())
}

/// Serve the replies in order; later requests repeat the last reply.
fn stub_sequence(replies: Vec<StubReply>) -> Stub {
    let replies = Arc::new(replies);
    stub_with(move |index| {
        let last = replies.len().saturating_sub(1);
        replies
            .get(index.min(last))
            .cloned()
            .unwrap_or_else(|| StubReply::ok("{}"))
    })
}

fn stub_with<F>(handler: F) -> Stub
where
    F: Fn(usize) -> StubReply + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").expect("stub bind");
    let addr = listener.local_addr().expect("stub addr");
    let requests: Arc<Mutex<Vec<RecordedRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = requests.clone();
    std::thread::spawn(move || {
        for (index, stream) in listener.incoming().flatten().enumerate() {
            let reply = handler(index);
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
        StubReply::Status(code, body) => write_response(&mut stream, code, &[], &body),
        StubReply::Raw {
            status,
            headers,
            body,
        } => write_response(&mut stream, status, &headers, &body),
    }
    let _ = stream.flush();
}

fn write_response(stream: &mut TcpStream, status: u16, headers: &[(String, String)], body: &str) {
    let reason = match status {
        200 => "OK",
        302 => "Found",
        401 => "Unauthorized",
        403 => "Forbidden",
        429 => "Too Many Requests",
        _ => "Error",
    };
    let mut response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in headers {
        response.push_str(&format!("{name}: {value}\r\n"));
    }
    response.push_str("\r\n");
    response.push_str(body);
    let _ = stream.write_all(response.as_bytes());
}

// ---------------------------------------------------------------------------
// Script harness
// ---------------------------------------------------------------------------

static AGENT_COUNTER: AtomicU64 = AtomicU64::new(0);

struct AgentDir(PathBuf);

impl AgentDir {
    fn new(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        let count = AGENT_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "rpi-usage-contract-{tag}-{}-{nanos}-{count}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("agent dir");
        AgentDir(dir)
    }

    fn write_auth(&self, body: &str) {
        std::fs::write(self.0.join("auth.json"), body).expect("auth.json");
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for AgentDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Debug)]
struct Run {
    status: Option<i32>,
    stdout: String,
    stderr: String,
}

/// python3 is a hard contract-test requirement (no silent skip).
fn python3() -> &'static str {
    static READY: OnceLock<()> = OnceLock::new();
    READY.get_or_init(|| {
        let probe = Command::new("python3")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        match probe {
            Ok(status) if status.success() => {}
            _ => panic!(
                "python3 is required to run the usage provider contract tests; \
                 install python3 and re-run `cargo test -p rpi-ext-usage --test scripts_contract`"
            ),
        }
    });
    "python3"
}

fn script_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("scripts")
        .join(name)
}

fn fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

/// Load a fixture and overwrite fields through JSON pointers.
fn fixture_with(name: &str, updates: &[(&str, Value)]) -> String {
    let mut value: Value = serde_json::from_str(&fixture(name)).expect("fixture json");
    for (pointer, new_value) in updates {
        set_pointer(&mut value, pointer, new_value.clone());
    }
    serde_json::to_string(&value).expect("fixture serialize")
}

fn set_pointer(value: &mut Value, pointer: &str, new_value: Value) {
    let (parent, key) = pointer
        .rsplit_once('/')
        .unwrap_or_else(|| panic!("pointer {pointer}"));
    let parent = if parent.is_empty() {
        value
    } else {
        value
            .pointer_mut(parent)
            .unwrap_or_else(|| panic!("pointer parent {parent}"))
    };
    match parent {
        Value::Array(array) => {
            let index: usize = key.parse().unwrap_or_else(|_| panic!("array index {key}"));
            array[index] = new_value;
        }
        Value::Object(map) => {
            map.insert(key.to_owned(), new_value);
        }
        _ => panic!("unsupported pointer {pointer}"),
    }
}

fn offset_epoch_ms(offset_secs: i64) -> Value {
    Value::from((FIXED_NOW_MS as i64 + offset_secs * 1000) as u64)
}

/// Run one script against a stub. `env` carries credentials and the pinned
/// clock; everything else the child sees is controlled by the harness.
fn run_script(
    script: &str,
    provider: &str,
    agent: &AgentDir,
    stub: &Stub,
    env: &[(&str, &str)],
    timeout_ms: u64,
) -> Run {
    let mut command = Command::new(python3());
    command
        .arg(script_path(script))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Remove every inherited credential/test variable first: `env_remove`
    // after `env` would unset the harness value again.
    for name in CREDENTIAL_ENVS {
        command.env_remove(name);
    }
    command
        .env("RPI_CODING_AGENT_DIR", agent.path())
        .env("RPI_USAGE_TIMEOUT_MS", timeout_ms.to_string())
        .env("RPI_USAGE_TEST_MODE", "1")
        .env("RPI_USAGE_TEST_BASE_URL", stub.origin());
    for (name, value) in env {
        command.env(name, value);
    }
    let mut child = command.spawn().expect("spawn python3");
    // Deliberately send a hostile baseUrl: first-party scripts must ignore it.
    let context = serde_json::json!({
        "provider": provider,
        "baseUrl": "https://attacker.invalid/usage",
    })
    .to_string();
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
    assert!(
        run.stderr.trim().is_empty(),
        "success wrote to stderr: {}",
        run.stderr
    );
    let value: Value = serde_json::from_str(run.stdout.trim())
        .unwrap_or_else(|error| panic!("invalid envelope: {error}: {}", run.stdout));
    validate_envelope(&value);
    value
}

fn assert_error(run: &Run, kind: &str) -> Value {
    assert_eq!(
        run.status,
        Some(2),
        "expected a classified exit 2: {:?}",
        run
    );
    let value: Value = serde_json::from_str(run.stdout.trim())
        .unwrap_or_else(|error| panic!("invalid error envelope: {error}: {}", run.stdout));
    assert_eq!(value["schemaVersion"], 1);
    assert!(
        value["provider"].as_str().is_some_and(|p| !p.is_empty()),
        "error envelope needs a provider: {value}"
    );
    assert_eq!(value["error"]["kind"], kind, "error envelope: {value}");
    value
}

fn assert_no_secret(run: &Run, secrets: &[&str]) {
    for secret in secrets {
        assert!(
            !run.stdout.contains(secret),
            "stdout leaked a credential: {}",
            run.stdout
        );
        assert!(
            !run.stderr.contains(secret),
            "stderr leaked a credential: {}",
            run.stderr
        );
    }
}

fn assert_no_credential_leak(run: &Run) {
    assert_no_secret(
        run,
        &[
            TEST_KEY,
            TEST_OAUTH_ACCESS,
            TEST_OAUTH_ACCOUNT,
            TEST_STORE_ACCESS,
            TEST_STORE_ACCOUNT,
            TEST_USER_ID,
            "test-resolved-secret",
        ],
    );
}

/// Every recorded request must be the documented read-only GET; no refresh,
/// consume, inference or other write endpoint may ever appear.
fn assert_read_only(requests: &[RecordedRequest]) {
    assert!(!requests.is_empty(), "the script sent no request");
    for request in requests {
        assert_eq!(request.method, "GET", "unexpected method: {request:?}");
        for forbidden in ["/messages", "consume", "refresh", "/oauth/token"] {
            assert!(
                !request.path.contains(forbidden),
                "forbidden request path {}",
                request.path
            );
        }
    }
}

fn is_rfc3339(value: &str) -> bool {
    let bytes = value.as_bytes();
    let digits = |range: std::ops::Range<usize>| {
        bytes
            .get(range)
            .is_some_and(|slice| slice.iter().all(u8::is_ascii_digit))
    };
    bytes.len() >= 20
        && digits(0..4)
        && bytes[4] == b'-'
        && digits(5..7)
        && bytes[7] == b'-'
        && digits(8..10)
        && bytes[10] == b'T'
        && digits(11..13)
        && bytes[13] == b':'
        && digits(14..16)
        && bytes[16] == b':'
        && digits(17..19)
        && (bytes[19] == b'Z'
            || (bytes.len() >= 25
                && (bytes[19] == b'+' || bytes[19] == b'-')
                && digits(20..22)
                && bytes[22] == b':'
                && digits(23..25)))
}

fn number(value: &Value, label: &str) -> f64 {
    value
        .as_f64()
        .unwrap_or_else(|| panic!("{label} must be a number: {value}"))
}

fn assert_amounts(object: &Value, label: &str) {
    for key in ["used", "total", "remaining"] {
        if let Some(value) = object.get(key) {
            let parsed = number(value, &format!("{label}:{key}"));
            assert!(parsed >= 0.0, "{label}: {key} must be non-negative");
            if key == "total" && parsed <= 0.0 {
                panic!("{label}: total must be positive when present");
            }
        }
    }
}

fn validate_envelope(value: &Value) {
    assert_eq!(value["schemaVersion"], 1);
    assert!(value["provider"].as_str().is_some_and(|p| !p.is_empty()));
    assert!(
        value["displayText"]
            .as_str()
            .is_some_and(|text| !text.trim().is_empty())
    );

    if let Some(windows) = value.get("windows").and_then(Value::as_array) {
        assert!(!windows.is_empty(), "windows must not be empty");
        let mut ids = HashSet::new();
        for window in windows {
            let id = window["id"].as_str().expect("window id");
            assert!(!id.trim().is_empty());
            assert!(ids.insert(id.to_owned()), "duplicate window id {id}");
            assert!(
                window["label"]
                    .as_str()
                    .is_some_and(|label| !label.trim().is_empty())
            );
            let status = window["status"]
                .as_str()
                .expect("window status is required");
            assert!(
                matches!(status, "ok" | "rate-limited" | "unknown"),
                "{id}: invalid status {status}"
            );
            let unlimited = window.get("unlimited").and_then(Value::as_bool) == Some(true);
            let percentage =
                window.get("usedPercent").is_some() || window.get("remainingPercent").is_some();
            let counted = window.get("used").is_some()
                || window.get("total").is_some()
                || window.get("remaining").is_some()
                || window.get("unit").is_some();
            let representations =
                usize::from(unlimited) + usize::from(percentage) + usize::from(counted);
            if status == "unknown" {
                // `unknown` windows may omit any representation; a value is
                // allowed but never treated as normal.
                assert!(
                    representations <= 1,
                    "{id}: at most one representation is allowed: {window}"
                );
            } else {
                assert_eq!(
                    representations, 1,
                    "{id}: exactly one window representation is required: {window}"
                );
            }
            if representations == 0 {
                // Unknown window without invented numbers.
            } else if unlimited {
                for key in [
                    "usedPercent",
                    "remainingPercent",
                    "used",
                    "total",
                    "remaining",
                    "unit",
                ] {
                    assert!(
                        window.get(key).is_none(),
                        "{id}: unlimited must not carry {key}"
                    );
                }
            } else if percentage {
                let used = number(&window["usedPercent"], "usedPercent");
                assert!(
                    (0.0..=100.0).contains(&used),
                    "{id}: usedPercent out of range"
                );
                if let Some(remaining) = window.get("remainingPercent") {
                    let remaining = number(remaining, "remainingPercent");
                    assert!((0.0..=100.0).contains(&remaining));
                    assert!(
                        (used + remaining - 100.0).abs() <= 1.0,
                        "{id}: used+remaining must be ~100"
                    );
                }
            } else {
                assert!(
                    window["unit"]
                        .as_str()
                        .is_some_and(|unit| !unit.trim().is_empty())
                );
                assert_amounts(window, id);
                assert!(
                    window.get("used").is_some()
                        || window.get("total").is_some()
                        || window.get("remaining").is_some(),
                    "{id}: counted window needs a number"
                );
            }
            if let Some(reset) = window.get("resetAt").and_then(Value::as_str) {
                assert!(
                    is_rfc3339(reset),
                    "{id}: resetAt must be RFC3339 with a timezone: {reset}"
                );
            }
            if let Some(duration) = window.get("durationSeconds") {
                assert!(
                    duration.as_u64().is_some_and(|seconds| seconds > 0),
                    "{id}: durationSeconds must be a positive integer"
                );
            }
        }
    }

    if let Some(ledgers) = value.get("ledgers").and_then(Value::as_array) {
        assert!(!ledgers.is_empty(), "ledgers must not be empty");
        let mut ids = HashSet::new();
        for ledger in ledgers {
            let id = ledger["id"].as_str().expect("ledger id");
            assert!(ids.insert(id.to_owned()), "duplicate ledger id {id}");
            assert!(
                ledger["label"]
                    .as_str()
                    .is_some_and(|label| !label.trim().is_empty())
            );
            let kind = ledger["kind"].as_str().expect("ledger kind");
            assert!(matches!(
                kind,
                "on-demand" | "prepaid" | "api-balance" | "quota"
            ));
            assert!(
                ledger["unit"]
                    .as_str()
                    .is_some_and(|unit| !unit.trim().is_empty())
            );
            if matches!(kind, "on-demand" | "prepaid" | "api-balance") {
                assert!(
                    ledger["currency"]
                        .as_str()
                        .is_some_and(|currency| !currency.trim().is_empty()),
                    "{id}: monetary ledger requires a currency"
                );
            }
            assert!(
                ledger.get("used").is_some()
                    || ledger.get("total").is_some()
                    || ledger.get("remaining").is_some(),
                "{id}: ledger needs a number"
            );
            assert_amounts(ledger, id);
        }
    }

    if let Some(balance) = value.get("balance").and_then(Value::as_array) {
        assert!(!balance.is_empty(), "balance must not be empty");
        for entry in balance {
            assert!(
                entry["currency"]
                    .as_str()
                    .is_some_and(|currency| !currency.trim().is_empty())
            );
        }
    }
}

// Credential presets.
const DEEPSEEK: [(&str, &str); 1] = [("DEEPSEEK_API_KEY", TEST_KEY)];
const GLM: [(&str, &str); 1] = [("ZAI_CODING_CN_API_KEY", TEST_KEY)];
const MINIMAX: [(&str, &str); 1] = [("MINIMAX_CN_API_KEY", TEST_KEY)];
const KIMI: [(&str, &str); 1] = [("KIMI_API_KEY", TEST_KEY)];
const OPENCODE: [(&str, &str); 1] = [("OPENCODE_API_KEY", TEST_KEY)];
const ANTHROPIC: [(&str, &str); 1] = [("RPI_USAGE_OAUTH_ACCESS", TEST_OAUTH_ACCESS)];
const CODEX: [(&str, &str); 2] = [
    ("RPI_USAGE_OAUTH_ACCESS", TEST_OAUTH_ACCESS),
    ("RPI_USAGE_OAUTH_ACCOUNT_ID", TEST_OAUTH_ACCOUNT),
];
const XAI: [(&str, &str); 1] = [("RPI_USAGE_OAUTH_ACCESS", TEST_OAUTH_ACCESS)];

/// Extend a credential preset with the pinned clock.
fn with_clock<'a>(env: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
    let mut combined = env.to_vec();
    combined.push(("RPI_USAGE_NOW_MS", "1800000000000"));
    combined
}

// ---------------------------------------------------------------------------
// Legacy providers (displayText byte-stable, typed windows added)
// ---------------------------------------------------------------------------

#[test]
fn deepseek_balance_is_typed_and_keeps_the_legacy_line() {
    let stub = stub_always(StubReply::ok(fixture("deepseek_balance.json")));
    let agent = AgentDir::new("deepseek");
    let run = run_script("deepseek.py", "deepseek", &agent, &stub, &DEEPSEEK, 5_000);
    let value = envelope(&run);
    assert_eq!(value["provider"], "deepseek");
    assert_eq!(value["displayText"], "CNY 12.50 / USD 3.00");
    assert_eq!(value["balance"][0]["currency"], "CNY");
    assert_eq!(value["balance"][0]["total"], 12.5);
    assert_eq!(value["balance"][0]["granted"], 0.0);
    assert_eq!(value["balance"][0]["toppedUp"], 12.5);
    assert_eq!(value["balance"][1]["currency"], "USD");
    assert!(
        value.get("windows").is_none(),
        "DeepSeek has no quota windows"
    );
    assert_no_credential_leak(&run);

    let requests = stub.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/user/balance");
    assert_eq!(requests[0].header("authorization"), Some("Bearer test-key"));
    assert_read_only(&requests);
}

#[test]
fn deepseek_unavailable_balance_stays_marked() {
    let body = fixture_with(
        "deepseek_balance.json",
        &[("/is_available", Value::Bool(false))],
    );
    let stub = stub_always(StubReply::ok(body));
    let agent = AgentDir::new("deepseek-unavailable");
    let run = run_script("deepseek.py", "deepseek", &agent, &stub, &DEEPSEEK, 5_000);
    let value = envelope(&run);
    assert_eq!(value["displayText"], "CNY 12.50 / USD 3.00 (unavailable)");
    assert_eq!(value["balance"].as_array().map(Vec::len), Some(2));
}

#[test]
fn glm_percent_windows_keep_the_legacy_line() {
    let body = fixture_with(
        "glm_quota_weekly.json",
        &[
            ("/data/limits/0/nextResetTime", offset_epoch_ms(338_400)),
            ("/data/limits/1/nextResetTime", offset_epoch_ms(2_880)),
        ],
    );
    let stub = stub_always(StubReply::ok(body));
    let agent = AgentDir::new("glm-percent");
    let run = run_script(
        "glm_coding_plan.py",
        "glm-coding-plan",
        &agent,
        &stub,
        &with_clock(&GLM),
        5_000,
    );
    let value = envelope(&run);
    assert_eq!(value["plan"], "pro");
    assert_eq!(
        value["displayText"],
        "5h 22%(~48m) / 7d 9%(~3d22h) / MCP 22%"
    );
    assert_eq!(value["quota"]["used"], 9.0);
    assert_eq!(value["quota"]["total"], 100.0);
    assert_eq!(value["used"], 9.0);
    assert_eq!(value["resetAt"], RESET_3D22H_ISO);

    let windows = value["windows"].as_array().expect("windows");
    assert_eq!(windows.len(), 3);
    assert_eq!(windows[0]["id"], "5h");
    assert_eq!(windows[0]["label"], "5h");
    assert_eq!(windows[0]["status"], "ok");
    assert_eq!(windows[0]["usedPercent"], 22.0);
    assert_eq!(windows[0]["remainingPercent"], 78.0);
    assert_eq!(windows[0]["resetAt"], RESET_48M_ISO);
    assert_eq!(windows[0]["durationSeconds"], 18_000);
    assert_eq!(windows[1]["id"], "7d");
    assert_eq!(windows[1]["usedPercent"], 9.0);
    assert_eq!(windows[1]["resetAt"], RESET_3D22H_ISO);
    assert_eq!(windows[1]["durationSeconds"], 604_800);
    assert_eq!(windows[2]["id"], "mcp");
    assert_eq!(windows[2]["label"], "MCP");
    assert_eq!(windows[2]["usedPercent"], 22.0);
    assert!(windows[2].get("durationSeconds").is_none());

    assert_no_credential_leak(&run);
    let requests = stub.requests();
    assert_eq!(requests[0].path, "/api/monitor/usage/quota/limit");
    assert_eq!(requests[0].header("authorization"), Some("Bearer test-key"));
    assert_read_only(&requests);
}

#[test]
fn glm_counted_windows_use_the_documented_units() {
    let body = fixture_with(
        "glm_count_only.json",
        &[("/data/limits/1/nextResetTime", offset_epoch_ms(338_400))],
    );
    let stub = stub_always(StubReply::ok(body));
    let agent = AgentDir::new("glm-counted");
    let run = run_script(
        "glm_coding_plan.py",
        "glm-coding-plan",
        &agent,
        &stub,
        &with_clock(&GLM),
        5_000,
    );
    let value = envelope(&run);
    // The legacy percent rule still derives 24% for the token window; the MCP
    // window has no total and is therefore only a counted typed window.
    assert_eq!(value["displayText"], "5h 24% / 7d 9%(~3d22h)");
    let windows = value["windows"].as_array().expect("windows");
    assert_eq!(windows[0]["id"], "5h");
    assert_eq!(windows[0]["unit"], "tokens");
    assert_eq!(windows[0]["used"], 1200.0);
    assert_eq!(windows[0]["total"], 5000.0);
    assert_eq!(windows[0]["remaining"], 3800.0);
    assert_eq!(windows[0]["status"], "ok");
    assert_eq!(windows[2]["id"], "mcp");
    assert_eq!(windows[2]["unit"], "requests");
    assert_eq!(windows[2]["used"], 12.0);
    assert_eq!(windows[2]["remaining"], 18.0);
    assert!(windows[2].get("total").is_none());
}

#[test]
fn glm_full_window_is_rate_limited() {
    let body = fixture_with(
        "glm_quota_weekly.json",
        &[("/data/limits/0/percentage", Value::from(100))],
    );
    let stub = stub_always(StubReply::ok(body));
    let agent = AgentDir::new("glm-limited");
    let run = run_script(
        "glm_coding_plan.py",
        "glm-coding-plan",
        &agent,
        &stub,
        &with_clock(&GLM),
        5_000,
    );
    let value = envelope(&run);
    let weekly = value["windows"]
        .as_array()
        .expect("windows")
        .iter()
        .find(|window| window["id"] == "7d")
        .expect("7d window");
    assert_eq!(weekly["status"], "rate-limited");
    assert_eq!(weekly["usedPercent"], 100.0);
}

#[test]
fn minimax_unlimited_and_percent_windows_are_typed() {
    let body = fixture_with(
        "minimax_unlimited.json",
        &[
            ("/model_remains/0/end_time", Value::from(RESET_2H5M_MS)),
            (
                "/model_remains/0/start_time",
                Value::from(RESET_2H5M_MS - 18_000_000),
            ),
            (
                "/model_remains/0/weekly_end_time",
                Value::from(RESET_3D22H_MS),
            ),
            (
                "/model_remains/0/weekly_start_time",
                Value::from(RESET_3D22H_MS - 604_800_000),
            ),
        ],
    );
    let stub = stub_always(StubReply::ok(body));
    let agent = AgentDir::new("minimax-unlimited");
    let run = run_script(
        "minimax_token_plan.py",
        "minimax-token-plan",
        &agent,
        &stub,
        &with_clock(&MINIMAX),
        5_000,
    );
    let value = envelope(&run);
    assert_eq!(value["displayText"], "5h unlimited / 7d 20%(~3d22h)");
    assert_eq!(value["plan"], "Token Plan");
    assert_eq!(value["quota"]["used"], 20.0);
    assert_eq!(value["resetAt"], RESET_3D22H_ISO);

    let windows = value["windows"].as_array().expect("windows");
    assert_eq!(windows[0]["id"], "interval");
    assert_eq!(windows[0]["label"], "5h");
    assert_eq!(windows[0]["status"], "ok");
    assert_eq!(windows[0]["unlimited"], true);
    assert_eq!(windows[0]["resetAt"], RESET_2H5M_ISO);
    assert_eq!(windows[0]["durationSeconds"], 18_000);
    assert_eq!(windows[1]["id"], "weekly");
    assert_eq!(windows[1]["label"], "7d");
    assert_eq!(windows[1]["usedPercent"], 20.0);
    assert_eq!(windows[1]["remainingPercent"], 80.0);
    assert_eq!(windows[1]["durationSeconds"], 604_800);
    assert_no_credential_leak(&run);
    let requests = stub.requests();
    assert_eq!(requests[0].path, "/v1/token_plan/remains");
    assert_eq!(requests[0].header("authorization"), Some("Bearer test-key"));
    assert_read_only(&requests);
}

#[test]
fn minimax_counted_window_is_used_when_percent_is_absent() {
    let body = fixture_with(
        "minimax_unlimited.json",
        &[
            (
                "/model_remains/0/current_weekly_remaining_percent",
                Value::Null,
            ),
            ("/model_remains/0/current_weekly_status", Value::from(1)),
        ],
    );
    let stub = stub_always(StubReply::ok(body));
    let agent = AgentDir::new("minimax-counted");
    let run = run_script(
        "minimax_token_plan.py",
        "minimax-token-plan",
        &agent,
        &stub,
        &with_clock(&MINIMAX),
        5_000,
    );
    let value = envelope(&run);
    assert_eq!(value["displayText"], "5h unlimited");
    let weekly = &value["windows"][1];
    assert_eq!(weekly["id"], "weekly");
    assert_eq!(weekly["unit"], "requests");
    assert_eq!(weekly["used"], 200.0);
    assert_eq!(weekly["total"], 1000.0);
    assert_eq!(weekly["remaining"], 800.0);
}

#[test]
fn minimax_exhausted_window_is_rate_limited() {
    let body = fixture_with(
        "minimax_unlimited.json",
        &[("/model_remains/0/current_interval_status", Value::from(2))],
    );
    let stub = stub_always(StubReply::ok(body));
    let agent = AgentDir::new("minimax-exhausted");
    let run = run_script(
        "minimax_token_plan.py",
        "minimax-token-plan",
        &agent,
        &stub,
        &with_clock(&MINIMAX),
        5_000,
    );
    let value = envelope(&run);
    assert_eq!(value["windows"][0]["id"], "interval");
    assert_eq!(value["windows"][0]["status"], "rate-limited");
    assert_eq!(value["windows"][0]["usedPercent"], 100.0);
}

#[test]
fn kimi_windows_and_booster_wallet_stay_separate() {
    let body = fixture_with(
        "kimi_usages_booster.json",
        &[
            ("/limits/0/detail/resetTime", Value::from(RESET_48M_ISO)),
            ("/usage/resetTime", Value::from(RESET_3D22H_ISO)),
        ],
    );
    let stub = stub_always(StubReply::ok(body));
    let agent = AgentDir::new("kimi-booster");
    let run = run_script(
        "kimi_code.py",
        "kimi-code",
        &agent,
        &stub,
        &with_clock(&KIMI),
        5_000,
    );
    let value = envelope(&run);
    // The booster is typed-only: the legacy quota line is unchanged.
    assert_eq!(value["displayText"], "5h 45%(~48m) / 7d 20%(~3d22h)");
    assert_eq!(value["plan"], "Intermediate");
    assert_eq!(value["quota"]["unit"], "requests");
    assert_eq!(value["quota"]["total"], 100.0);
    assert_eq!(value["quota"]["used"], 20.0);
    assert_eq!(value["used"], 20.0);

    let windows = value["windows"].as_array().expect("windows");
    assert_eq!(windows.len(), 2, "the booster is not a quota window");
    assert_eq!(windows[0]["id"], "5h");
    assert_eq!(windows[0]["label"], "5h");
    assert_eq!(windows[0]["usedPercent"], 45.0);
    assert_eq!(windows[0]["resetAt"], RESET_48M_ISO);
    assert_eq!(windows[0]["durationSeconds"], 18_000);
    assert_eq!(windows[1]["id"], "7d");
    assert_eq!(windows[1]["label"], "7d");
    assert_eq!(windows[1]["usedPercent"], 20.0);
    assert_eq!(windows[1]["resetAt"], RESET_3D22H_ISO);
    assert_eq!(windows[1]["durationSeconds"], 604_800);

    let ledgers = value["ledgers"].as_array().expect("ledgers");
    assert_eq!(ledgers.len(), 1);
    assert_eq!(ledgers[0]["id"], "booster_wallet");
    assert_eq!(ledgers[0]["label"], "Booster");
    assert_eq!(ledgers[0]["kind"], "prepaid");
    assert_eq!(ledgers[0]["unit"], "USD");
    assert_eq!(ledgers[0]["currency"], "USD");
    assert_eq!(ledgers[0]["remaining"], 12.5);
    assert_eq!(ledgers[0]["total"], 50.0);
    assert_eq!(ledgers[0]["used"], 37.5);

    let requests = stub.requests();
    assert_eq!(requests[0].path, "/coding/v1/usages");
    assert_eq!(requests[0].header("x-api-key"), Some("test-key"));
    assert_eq!(requests[0].header("authorization"), None);
    assert_read_only(&requests);
}

#[test]
fn kimi_full_window_is_rate_limited() {
    let body = fixture_with(
        "kimi_usages_used.json",
        &[
            ("/usage/remaining", Value::from("0")),
            ("/limits/0/detail/resetTime", Value::from(RESET_48M_ISO)),
            ("/usage/resetTime", Value::from(RESET_3D22H_ISO)),
        ],
    );
    let stub = stub_always(StubReply::ok(body));
    let agent = AgentDir::new("kimi-limited");
    let run = run_script(
        "kimi_code.py",
        "kimi-code",
        &agent,
        &stub,
        &with_clock(&KIMI),
        5_000,
    );
    let value = envelope(&run);
    assert_eq!(value["windows"][1]["id"], "7d");
    assert_eq!(value["windows"][1]["status"], "rate-limited");
    assert_eq!(value["windows"][1]["usedPercent"], 100.0);
    assert_eq!(value["displayText"], "5h 45%(~48m) / 7d 100%(~3d22h)");
}

#[test]
fn deepseek_zero_balance_and_field_drift() {
    let body = fixture_with(
        "deepseek_balance.json",
        &[
            ("/balance_infos/0/total_balance", Value::from("0.00")),
            ("/balance_infos/0/granted_balance", Value::from("0.00")),
            ("/balance_infos/0/topped_up_balance", Value::from("0.00")),
        ],
    );
    let stub = stub_always(StubReply::ok(body));
    let agent = AgentDir::new("deepseek-zero");
    let run = run_script("deepseek.py", "deepseek", &agent, &stub, &DEEPSEEK, 5_000);
    let value = envelope(&run);
    assert_eq!(value["displayText"], "CNY 0.00 / USD 3.00");
    assert_eq!(value["balance"][0]["total"], 0.0);
    assert_eq!(value["balance"][0]["granted"], 0.0);
    assert_eq!(value["balance"][0]["toppedUp"], 0.0);

    // Field drift: a response without balance_infos is a shape change.
    let body = fixture_with("deepseek_balance.json", &[("/balance_infos", Value::Null)]);
    let stub = stub_always(StubReply::ok(body));
    let agent = AgentDir::new("deepseek-drift");
    let run = run_script("deepseek.py", "deepseek", &agent, &stub, &DEEPSEEK, 5_000);
    assert_error(&run, "shape-changed");
    assert_eq!(stub.requests().len(), 1);
}

#[test]
fn glm_zero_usage_partial_windows_and_invalid_percentage() {
    // Zero usage on the weekly window.
    let body = fixture_with(
        "glm_quota_weekly.json",
        &[("/data/limits/0/percentage", Value::from(0))],
    );
    let stub = stub_always(StubReply::ok(body));
    let agent = AgentDir::new("glm-zero");
    let run = run_script(
        "glm_coding_plan.py",
        "glm-coding-plan",
        &agent,
        &stub,
        &with_clock(&GLM),
        5_000,
    );
    let value = envelope(&run);
    let weekly = value["windows"]
        .as_array()
        .expect("windows")
        .iter()
        .find(|window| window["id"] == "7d")
        .expect("7d window");
    assert_eq!(weekly["usedPercent"], 0.0);
    assert_eq!(weekly["remainingPercent"], 100.0);
    assert_eq!(weekly["status"], "ok");

    // Partial windows: only the weekly limit is present.
    let body = fixture_with(
        "glm_quota_weekly.json",
        &[(
            "/data/limits",
            serde_json::json!([{
                "type": "TOKENS_LIMIT",
                "unit": 6,
                "number": 1,
                "percentage": 9
            }]),
        )],
    );
    let stub = stub_always(StubReply::ok(body));
    let agent = AgentDir::new("glm-partial");
    let run = run_script(
        "glm_coding_plan.py",
        "glm-coding-plan",
        &agent,
        &stub,
        &with_clock(&GLM),
        5_000,
    );
    let value = envelope(&run);
    assert_eq!(value["displayText"], "7d 9%");
    let windows = value["windows"].as_array().expect("windows");
    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0]["id"], "7d");

    // An invalid percentage on the only window is a classified shape change.
    let body = fixture_with(
        "glm_quota_weekly.json",
        &[(
            "/data/limits",
            serde_json::json!([{
                "type": "TOKENS_LIMIT",
                "unit": 3,
                "number": 5,
                "percentage": "abc"
            }]),
        )],
    );
    let stub = stub_always(StubReply::ok(body));
    let agent = AgentDir::new("glm-invalid-percent");
    let run = run_script(
        "glm_coding_plan.py",
        "glm-coding-plan",
        &agent,
        &stub,
        &with_clock(&GLM),
        5_000,
    );
    assert_error(&run, "shape-changed");
}

#[test]
fn minimax_zero_usage_partial_windows_and_invalid_percent() {
    // Zero usage on both windows.
    let body = fixture_with(
        "minimax_remains.json",
        &[
            (
                "/model_remains/0/current_interval_remaining_percent",
                Value::from(100),
            ),
            (
                "/model_remains/0/current_weekly_remaining_percent",
                Value::from(100),
            ),
            ("/model_remains/0/current_weekly_status", Value::from(1)),
        ],
    );
    let stub = stub_always(StubReply::ok(body));
    let agent = AgentDir::new("minimax-zero");
    let run = run_script(
        "minimax_token_plan.py",
        "minimax-token-plan",
        &agent,
        &stub,
        &with_clock(&MINIMAX),
        5_000,
    );
    let value = envelope(&run);
    for window in value["windows"].as_array().expect("windows") {
        assert_eq!(window["usedPercent"], 0.0, "{window}");
    }

    // Partial windows: the weekly bucket has no usable representation.
    let body = fixture_with(
        "minimax_remains.json",
        &[
            (
                "/model_remains/0/current_weekly_remaining_percent",
                Value::Null,
            ),
            ("/model_remains/0/current_weekly_status", Value::Null),
        ],
    );
    let stub = stub_always(StubReply::ok(body));
    let agent = AgentDir::new("minimax-partial");
    let run = run_script(
        "minimax_token_plan.py",
        "minimax-token-plan",
        &agent,
        &stub,
        &with_clock(&MINIMAX),
        5_000,
    );
    let value = envelope(&run);
    let windows = value["windows"].as_array().expect("windows");
    assert_eq!(windows.len(), 2);
    let interval = windows
        .iter()
        .find(|window| window["id"] == "interval")
        .expect("interval window");
    let weekly = windows
        .iter()
        .find(|window| window["id"] == "weekly")
        .expect("weekly window");
    assert_eq!(interval["status"], "ok");
    assert_eq!(weekly["status"], "unknown");
    assert!(
        value["displayText"]
            .as_str()
            .is_some_and(|text| text.starts_with("5h ")),
        "{value}"
    );

    // An invalid interval percentage keeps the still-valid weekly window.
    let body = fixture_with(
        "minimax_unlimited.json",
        &[
            (
                "/model_remains/0/current_interval_remaining_percent",
                Value::from("abc"),
            ),
            ("/model_remains/0/current_interval_status", Value::from(1)),
        ],
    );
    let stub = stub_always(StubReply::ok(body));
    let agent = AgentDir::new("minimax-invalid");
    let run = run_script(
        "minimax_token_plan.py",
        "minimax-token-plan",
        &agent,
        &stub,
        &with_clock(&MINIMAX),
        5_000,
    );
    let value = envelope(&run);
    let windows = value["windows"].as_array().expect("windows");
    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0]["id"], "weekly");
}

#[test]
fn kimi_partial_and_invalid_ratio() {
    // Partial windows: no rolling limit, no 5h ratio -> only the 7d quota.
    let body = fixture_with(
        "kimi_usages.json",
        &[
            ("/limits", Value::Array(Vec::new())),
            ("/usages/limit_5h", Value::Null),
        ],
    );
    let stub = stub_always(StubReply::ok(body));
    let agent = AgentDir::new("kimi-partial");
    let run = run_script(
        "kimi_code.py",
        "kimi-code",
        &agent,
        &stub,
        &with_clock(&KIMI),
        5_000,
    );
    let value = envelope(&run);
    let windows = value["windows"].as_array().expect("windows");
    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0]["id"], "7d");

    // An invalid 7d ratio keeps the valid 5h window instead of panicking.
    let body = fixture_with(
        "kimi_usages_used.json",
        &[
            ("/usage", Value::Null),
            ("/limits", Value::Array(Vec::new())),
            ("/usages/limit_7d/used_ratio", Value::from("abc")),
        ],
    );
    let stub = stub_always(StubReply::ok(body));
    let agent = AgentDir::new("kimi-invalid");
    let run = run_script(
        "kimi_code.py",
        "kimi-code",
        &agent,
        &stub,
        &with_clock(&KIMI),
        5_000,
    );
    let value = envelope(&run);
    let windows = value["windows"].as_array().expect("windows");
    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0]["id"], "5h");

    // An out-of-range ratio is invalid too (not clamped to 100%).
    let body = fixture_with(
        "kimi_usages_used.json",
        &[
            ("/usage", Value::Null),
            ("/limits", Value::Array(Vec::new())),
            ("/usages/limit_7d/used_ratio", Value::from(150)),
        ],
    );
    let stub = stub_always(StubReply::ok(body));
    let agent = AgentDir::new("kimi-out-of-range");
    let run = run_script(
        "kimi_code.py",
        "kimi-code",
        &agent,
        &stub,
        &with_clock(&KIMI),
        5_000,
    );
    let value = envelope(&run);
    let windows = value["windows"].as_array().expect("windows");
    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0]["id"], "5h");

    // No usable windows at all is a classified shape change.
    let body = fixture_with(
        "kimi_usages.json",
        &[
            ("/usage", Value::Null),
            ("/limits", Value::Array(Vec::new())),
            ("/usages", Value::Null),
        ],
    );
    let stub = stub_always(StubReply::ok(body));
    let agent = AgentDir::new("kimi-empty");
    let run = run_script(
        "kimi_code.py",
        "kimi-code",
        &agent,
        &stub,
        &with_clock(&KIMI),
        5_000,
    );
    assert_error(&run, "shape-changed");
}

#[test]
fn minimax_unknown_status_is_not_ok() {
    let body = fixture_with(
        "minimax_remains.json",
        &[("/model_remains/0/current_interval_status", Value::from(4))],
    );
    let stub = stub_always(StubReply::ok(body));
    let agent = AgentDir::new("minimax-unknown");
    let run = run_script(
        "minimax_token_plan.py",
        "minimax-token-plan",
        &agent,
        &stub,
        &with_clock(&MINIMAX),
        5_000,
    );
    let value = envelope(&run);
    let interval = value["windows"]
        .as_array()
        .expect("windows")
        .iter()
        .find(|window| window["id"] == "interval")
        .expect("interval window");
    assert_eq!(interval["status"], "unknown");
    assert!(interval.get("usedPercent").is_none());
    assert!(interval.get("unit").is_none());
    // The still-valid weekly window is kept; the unknown one is not rendered.
    assert_eq!(value["displayText"], "7d unlimited");
}

#[test]
fn minimax_all_unknown_or_invalid_percent_is_shape_changed() {
    // Both statuses undocumented -> no usable window.
    let body = fixture_with(
        "minimax_remains.json",
        &[
            ("/model_remains/0/current_interval_status", Value::from(4)),
            ("/model_remains/0/current_weekly_status", Value::from(4)),
        ],
    );
    let stub = stub_always(StubReply::ok(body));
    let agent = AgentDir::new("minimax-all-unknown");
    let run = run_script(
        "minimax_token_plan.py",
        "minimax-token-plan",
        &agent,
        &stub,
        &with_clock(&MINIMAX),
        5_000,
    );
    assert_error(&run, "shape-changed");

    // An out-of-range remaining percentage is invalid, not clamped.
    let body = fixture_with(
        "minimax_unlimited.json",
        &[
            (
                "/model_remains/0/current_interval_remaining_percent",
                Value::from(150),
            ),
            ("/model_remains/0/current_interval_status", Value::from(1)),
            ("/model_remains/0/current_weekly_status", Value::from(4)),
        ],
    );
    let stub = stub_always(StubReply::ok(body));
    let agent = AgentDir::new("minimax-invalid-range");
    let run = run_script(
        "minimax_token_plan.py",
        "minimax-token-plan",
        &agent,
        &stub,
        &with_clock(&MINIMAX),
        5_000,
    );
    assert_error(&run, "shape-changed");
}

#[test]
fn opencode_go_all_unknown_and_zero_partial_windows() {
    let body = fixture_with(
        "opencode_go_usage.json",
        &[
            ("/usage/rolling/status", Value::from("mystery-state")),
            ("/usage/weekly/status", Value::from("mystery-state")),
            ("/usage/monthly/status", Value::from("mystery-state")),
        ],
    );
    let stub = stub_always(StubReply::ok(body));
    let agent = AgentDir::new("opencode-all-unknown");
    let run = run_script(
        "opencode_go.py",
        "opencode-go",
        &agent,
        &stub,
        &with_clock(&OPENCODE),
        5_000,
    );
    assert_error(&run, "shape-changed");

    // Zero use on a partial response (only rolling is present).
    let body = fixture_with(
        "opencode_go_usage.json",
        &[
            ("/usage/rolling/percent", Value::from(0)),
            ("/usage/weekly", Value::Null),
            ("/usage/monthly", Value::Null),
        ],
    );
    let stub = stub_always(StubReply::ok(body));
    let agent = AgentDir::new("opencode-zero");
    let run = run_script(
        "opencode_go.py",
        "opencode-go",
        &agent,
        &stub,
        &with_clock(&OPENCODE),
        5_000,
    );
    let value = envelope(&run);
    assert!(
        value["displayText"]
            .as_str()
            .is_some_and(|text| text.starts_with("rolling 0%")),
        "{value}"
    );
    let windows = value["windows"].as_array().expect("windows");
    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0]["usedPercent"], 0.0);
}

#[test]
fn anthropic_zero_partial_and_rate_limited_windows() {
    let body = fixture_with(
        "anthropic_usage.json",
        &[
            ("/five_hour/utilization", Value::from(0)),
            ("/seven_day", Value::Null),
        ],
    );
    let stub = stub_always(StubReply::ok(body));
    let agent = AgentDir::new("anthropic-zero");
    let run = run_script(
        "anthropic.py",
        "anthropic",
        &agent,
        &stub,
        &with_clock(&ANTHROPIC),
        5_000,
    );
    let value = envelope(&run);
    assert!(
        value["displayText"]
            .as_str()
            .is_some_and(|text| text.starts_with("5h 0%")),
        "{value}"
    );
    let windows = value["windows"].as_array().expect("windows");
    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0]["usedPercent"], 0.0);

    let body = fixture_with(
        "anthropic_usage.json",
        &[("/five_hour/utilization", Value::from(100))],
    );
    let stub = stub_always(StubReply::ok(body));
    let agent = AgentDir::new("anthropic-limited");
    let run = run_script(
        "anthropic.py",
        "anthropic",
        &agent,
        &stub,
        &with_clock(&ANTHROPIC),
        5_000,
    );
    let value = envelope(&run);
    assert_eq!(value["windows"][0]["status"], "rate-limited");
    assert_eq!(value["windows"][0]["usedPercent"], 100.0);
}

#[test]
fn codex_zero_partial_all_unknown_and_invalid_percent() {
    let body = fixture_with(
        "openai_codex_usage.json",
        &[
            ("/rate_limit/primary_window/used_percent", Value::from(0)),
            ("/rate_limit/secondary_window", Value::Null),
        ],
    );
    let stub = stub_always(StubReply::ok(body));
    let agent = AgentDir::new("codex-zero");
    let run = run_script(
        "openai_codex.py",
        "openai-codex",
        &agent,
        &stub,
        &with_clock(&CODEX),
        5_000,
    );
    let value = envelope(&run);
    assert!(
        value["displayText"]
            .as_str()
            .is_some_and(|text| text.starts_with("5h 0%")),
        "{value}"
    );
    let windows = value["windows"].as_array().expect("windows");
    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0]["usedPercent"], 0.0);

    let body = fixture_with(
        "openai_codex_usage.json",
        &[
            ("/rate_limit/allowed", Value::Null),
            ("/rate_limit/limit_reached", Value::Bool(false)),
        ],
    );
    let stub = stub_always(StubReply::ok(body));
    let agent = AgentDir::new("codex-all-unknown");
    let run = run_script(
        "openai_codex.py",
        "openai-codex",
        &agent,
        &stub,
        &with_clock(&CODEX),
        5_000,
    );
    assert_error(&run, "shape-changed");

    let body = fixture_with(
        "openai_codex_usage.json",
        &[(
            "/rate_limit/primary_window/used_percent",
            Value::from("abc"),
        )],
    );
    let stub = stub_always(StubReply::ok(body));
    let agent = AgentDir::new("codex-invalid-percent");
    let run = run_script(
        "openai_codex.py",
        "openai-codex",
        &agent,
        &stub,
        &with_clock(&CODEX),
        5_000,
    );
    assert_error(&run, "shape-changed");
}

#[test]
fn glm_out_of_range_percentage_is_shape_changed() {
    for value in [Value::from(150), Value::from(-10)] {
        let body = fixture_with(
            "glm_quota_weekly.json",
            &[(
                "/data/limits",
                serde_json::json!([{
                    "type": "TOKENS_LIMIT",
                    "unit": 3,
                    "number": 5,
                    "percentage": value
                }]),
            )],
        );
        let stub = stub_always(StubReply::ok(body));
        let agent = AgentDir::new("glm-out-of-range");
        let run = run_script(
            "glm_coding_plan.py",
            "glm-coding-plan",
            &agent,
            &stub,
            &with_clock(&GLM),
            5_000,
        );
        assert_error(&run, "shape-changed");
    }
}

#[test]
fn kimi_zero_usage_is_typed() {
    let body = fixture_with(
        "kimi_usages_used.json",
        &[
            ("/usages/limit_5h/used_ratio", Value::from(0)),
            ("/usages/limit_7d/used_ratio", Value::from(0)),
            ("/usage/remaining", Value::from("100")),
            ("/limits/0/detail/remaining", Value::from("100")),
        ],
    );
    let stub = stub_always(StubReply::ok(body));
    let agent = AgentDir::new("kimi-zero");
    let run = run_script(
        "kimi_code.py",
        "kimi-code",
        &agent,
        &stub,
        &with_clock(&KIMI),
        5_000,
    );
    let value = envelope(&run);
    for window in value["windows"].as_array().expect("windows") {
        assert_eq!(window["usedPercent"], 0.0, "{window}");
    }
    let text = value["displayText"].as_str().expect("displayText");
    assert!(text.starts_with("5h 0%"), "{text}");
    assert!(text.contains("7d 0%"), "{text}");
}

#[test]
fn xai_rate_limited_window_is_not_ok() {
    let billing = fixture_with(
        "xai_billing.json",
        &[("/config/creditUsagePercent", Value::from(100))],
    );
    let stub = stub_sequence(vec![
        StubReply::ok(fixture("xai_user.json")),
        StubReply::ok(billing),
    ]);
    let agent = AgentDir::new("xai-limited");
    let run = run_script("xai.py", "xai", &agent, &stub, &with_clock(&XAI), 5_000);
    let value = envelope(&run);
    assert_eq!(value["windows"][0]["status"], "rate-limited");
    assert_eq!(value["windows"][0]["usedPercent"], 100.0);
}

// ---------------------------------------------------------------------------
// New providers
// ---------------------------------------------------------------------------

#[test]
fn opencode_go_windows_keep_the_rolling_identity() {
    let body = fixture_with(
        "opencode_go_usage.json",
        &[
            ("/usage/rolling/resetsAt", Value::from(RESET_48M_ISO)),
            ("/usage/weekly/resetsAt", Value::from(RESET_3D22H_ISO)),
            ("/usage/monthly/resetsAt", Value::from(RESET_2H5M_ISO)),
        ],
    );
    let stub = stub_always(StubReply::ok(body));
    let agent = AgentDir::new("opencode");
    let run = run_script(
        "opencode_go.py",
        "opencode-go",
        &agent,
        &stub,
        &with_clock(&OPENCODE),
        5_000,
    );
    let value = envelope(&run);
    // `rolling` is never renamed to 5h, even with a 48m reset.
    assert_eq!(
        value["displayText"],
        "rolling 12.5%(~48m) / weekly 68%(~3d22h) / monthly 100%(~2h5m)"
    );
    let windows = value["windows"].as_array().expect("windows");
    assert_eq!(windows.len(), 3);
    assert_eq!(windows[0]["id"], "rolling");
    assert_eq!(windows[0]["label"], "rolling");
    assert_eq!(windows[0]["status"], "ok");
    assert_eq!(windows[0]["usedPercent"], 12.5);
    assert_eq!(windows[0]["remainingPercent"], 87.5);
    assert_eq!(windows[0]["resetAt"], RESET_48M_ISO);
    assert_eq!(windows[1]["id"], "weekly");
    assert_eq!(windows[1]["usedPercent"], 68.0);
    assert_eq!(windows[1]["resetAt"], RESET_3D22H_ISO);
    assert_eq!(windows[2]["id"], "monthly");
    assert_eq!(windows[2]["status"], "rate-limited");
    assert_eq!(windows[2]["usedPercent"], 100.0);

    assert_no_credential_leak(&run);
    let requests = stub.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/zen/go/v1/usage");
    assert_eq!(requests[0].header("authorization"), Some("Bearer test-key"));
    assert_eq!(requests[0].header("x-opencode-org-id"), None);
    assert_read_only(&requests);
}

#[test]
fn opencode_go_unknown_status_is_not_ok() {
    let body = fixture_with(
        "opencode_go_usage.json",
        &[("/usage/rolling/status", Value::from("quota-paused"))],
    );
    let stub = stub_always(StubReply::ok(body));
    let agent = AgentDir::new("opencode-unknown");
    let run = run_script(
        "opencode_go.py",
        "opencode-go",
        &agent,
        &stub,
        &with_clock(&OPENCODE),
        5_000,
    );
    let value = envelope(&run);
    assert_eq!(value["windows"][0]["id"], "rolling");
    assert_eq!(value["windows"][0]["status"], "unknown");
    assert_eq!(value["windows"][1]["status"], "ok");
}

#[test]
fn opencode_go_shape_drift_fails_closed() {
    for updates in [
        vec![("/usage/rolling/status", Value::Null)],
        vec![("/usage/rolling/percent", Value::Null)],
        vec![("/usage/rolling/percent", Value::from(150))],
        vec![(
            "/usage/rolling/resetsAt",
            Value::from("2026-13-40T99:99:99Z"),
        )],
        vec![("/usage/rolling/resetsAt", Value::from("not-a-time"))],
    ] {
        let stub = stub_always(StubReply::ok(fixture_with(
            "opencode_go_usage.json",
            &updates,
        )));
        let agent = AgentDir::new("opencode-shape");
        let run = run_script(
            "opencode_go.py",
            "opencode-go",
            &agent,
            &stub,
            &with_clock(&OPENCODE),
            5_000,
        );
        assert_error(&run, "shape-changed");
    }
}

#[test]
fn anthropic_dual_windows_are_typed() {
    let body = fixture_with(
        "anthropic_usage.json",
        &[
            ("/five_hour/resets_at", Value::from(RESET_48M_ISO)),
            ("/seven_day/resets_at", Value::from(RESET_3D22H_ISO)),
        ],
    );
    let stub = stub_always(StubReply::ok(body));
    let agent = AgentDir::new("anthropic");
    let run = run_script(
        "anthropic.py",
        "anthropic",
        &agent,
        &stub,
        &with_clock(&ANTHROPIC),
        5_000,
    );
    let value = envelope(&run);
    assert_eq!(value["displayText"], "5h 39%(~48m) / 7d 31.5%(~3d22h)");
    let windows = value["windows"].as_array().expect("windows");
    assert_eq!(windows[0]["id"], "five_hour");
    assert_eq!(windows[0]["label"], "5h");
    assert_eq!(windows[0]["usedPercent"], 39.0);
    assert_eq!(windows[0]["remainingPercent"], 61.0);
    assert_eq!(windows[0]["resetAt"], RESET_48M_ISO);
    assert_eq!(windows[0]["durationSeconds"], 18_000);
    assert_eq!(windows[1]["id"], "seven_day");
    assert_eq!(windows[1]["label"], "7d");
    assert_eq!(windows[1]["usedPercent"], 31.5);
    assert_eq!(windows[1]["resetAt"], RESET_3D22H_ISO);
    assert_eq!(windows[1]["durationSeconds"], 604_800);

    assert_no_credential_leak(&run);
    let requests = stub.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/api/oauth/usage");
    assert_eq!(
        requests[0].header("authorization"),
        Some("Bearer test-oauth-access")
    );
    assert_eq!(
        requests[0].header("anthropic-beta"),
        Some("oauth-2025-04-20")
    );
    assert_eq!(requests[0].header("x-api-key"), None);
    assert_read_only(&requests);
}

#[test]
fn anthropic_api_key_alone_is_not_a_subscription_credential() {
    let stub = stub_always(StubReply::ok(fixture("anthropic_usage.json")));
    let agent = AgentDir::new("anthropic-api-key");
    let run = run_script(
        "anthropic.py",
        "anthropic",
        &agent,
        &stub,
        &with_clock(&[("ANTHROPIC_API_KEY", TEST_KEY)]),
        5_000,
    );
    assert_error(&run, "auth-unavailable");
    assert_no_credential_leak(&run);
    assert!(stub.requests().is_empty(), "no request without OAuth");
}

#[test]
fn anthropic_expired_auth_store_entry_fails_closed() {
    let agent = AgentDir::new("anthropic-expired");
    agent.write_auth(&format!(
        r#"{{"anthropic":{{"type":"oauth","access":"{TEST_STORE_ACCESS}","refresh":"test-refresh","expires":{}}}}}"#,
        FIXED_NOW_MS - 1000
    ));
    let stub = stub_always(StubReply::ok(fixture("anthropic_usage.json")));
    let run = run_script(
        "anthropic.py",
        "anthropic",
        &agent,
        &stub,
        &with_clock(&[]),
        5_000,
    );
    assert_error(&run, "auth-unavailable");
    assert!(
        stub.requests().is_empty(),
        "no request with an expired access"
    );
}

#[test]
fn anthropic_reads_the_unexpired_auth_store_access() {
    let agent = AgentDir::new("anthropic-store");
    agent.write_auth(&format!(
        r#"{{"anthropic":{{"type":"oauth","access":"{TEST_STORE_ACCESS}","refresh":"test-refresh","expires":{}}}}}"#,
        FIXED_NOW_MS + 3_600_000
    ));
    let stub = stub_always(StubReply::ok(fixture("anthropic_usage.json")));
    let run = run_script(
        "anthropic.py",
        "anthropic",
        &agent,
        &stub,
        &with_clock(&[]),
        5_000,
    );
    let _ = envelope(&run);
    let requests = stub.requests();
    assert_eq!(
        requests[0].header("authorization"),
        Some("Bearer test-store-access")
    );
    assert_no_credential_leak(&run);
}

#[test]
fn anthropic_shape_drift_fails_closed() {
    for updates in [
        vec![("/five_hour/utilization", Value::Null)],
        vec![("/five_hour/utilization", Value::from(101))],
        vec![("/five_hour/utilization", Value::from("39"))],
        vec![("/five_hour/resets_at", Value::from("tomorrow"))],
    ] {
        let stub = stub_always(StubReply::ok(fixture_with(
            "anthropic_usage.json",
            &updates,
        )));
        let agent = AgentDir::new("anthropic-shape");
        let run = run_script(
            "anthropic.py",
            "anthropic",
            &agent,
            &stub,
            &with_clock(&ANTHROPIC),
            5_000,
        );
        assert_error(&run, "shape-changed");
    }
}

#[test]
fn codex_windows_map_reset_epoch_and_the_reset_after_fallback() {
    let stub = stub_always(StubReply::ok(fixture("openai_codex_usage.json")));
    let agent = AgentDir::new("codex");
    let run = run_script(
        "openai_codex.py",
        "openai-codex",
        &agent,
        &stub,
        &with_clock(&CODEX),
        5_000,
    );
    let value = envelope(&run);
    assert_eq!(value["plan"], "pro");
    // primary reset_at is epoch seconds; secondary uses reset_after_seconds
    // from the pinned clock.
    assert_eq!(value["displayText"], "5h 42.5%(~48m) / 7d 31%(~3d22h)");
    let windows = value["windows"].as_array().expect("windows");
    assert_eq!(windows[0]["id"], "primary");
    assert_eq!(windows[0]["label"], "5h");
    assert_eq!(windows[0]["status"], "ok");
    assert_eq!(windows[0]["usedPercent"], 42.5);
    assert_eq!(windows[0]["remainingPercent"], 57.5);
    assert_eq!(windows[0]["resetAt"], RESET_48M_ISO);
    assert_eq!(windows[0]["durationSeconds"], 18_000);
    assert_eq!(windows[1]["id"], "secondary");
    assert_eq!(windows[1]["label"], "7d");
    assert_eq!(windows[1]["usedPercent"], 31.0);
    assert_eq!(windows[1]["resetAt"], RESET_3D22H_ISO);
    assert_eq!(windows[1]["durationSeconds"], 604_800);

    assert_no_credential_leak(&run);
    let requests = stub.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/backend-api/wham/usage");
    assert_eq!(
        requests[0].header("authorization"),
        Some("Bearer test-oauth-access")
    );
    assert_eq!(
        requests[0].header("chatgpt-account-id"),
        Some("test-account-id")
    );
    assert_read_only(&requests);
}

#[test]
fn codex_status_only_trusts_explicit_allowed_flags() {
    for (allowed, limit_reached, expected) in [
        (Value::Bool(false), Value::Bool(false), "rate-limited"),
        (Value::Bool(true), Value::Bool(true), "rate-limited"),
        (Value::Bool(true), Value::Bool(false), "ok"),
    ] {
        let body = fixture_with(
            "openai_codex_usage.json",
            &[
                ("/rate_limit/allowed", allowed),
                ("/rate_limit/limit_reached", limit_reached),
            ],
        );
        let stub = stub_always(StubReply::ok(body));
        let agent = AgentDir::new("codex-status");
        let run = run_script(
            "openai_codex.py",
            "openai-codex",
            &agent,
            &stub,
            &with_clock(&CODEX),
            5_000,
        );
        let value = envelope(&run);
        assert_eq!(value["windows"][0]["status"], expected);
        assert_eq!(value["windows"][1]["status"], expected);
    }
}

#[test]
fn codex_injected_access_never_mixes_in_a_store_account() {
    // The host injects access and accountId together. An injected access
    // without an account must fail closed instead of borrowing an accountId
    // from a different local credential entry.
    let agent = AgentDir::new("codex-no-account");
    agent.write_auth(&format!(
        r#"{{"openai-codex":{{"type":"oauth","access":"{TEST_STORE_ACCESS}","refresh":"test-refresh","expires":{},"accountId":"{TEST_STORE_ACCOUNT}"}}}}"#,
        FIXED_NOW_MS + 3_600_000
    ));
    let stub = stub_always(StubReply::ok(fixture("openai_codex_usage.json")));
    let run = run_script(
        "openai_codex.py",
        "openai-codex",
        &agent,
        &stub,
        &with_clock(&[("RPI_USAGE_OAUTH_ACCESS", TEST_OAUTH_ACCESS)]),
        5_000,
    );
    assert_error(&run, "auth-unavailable");
    assert!(stub.requests().is_empty());
}

#[test]
fn codex_reads_top_level_auth_store_access_and_account() {
    let agent = AgentDir::new("codex-store");
    agent.write_auth(&format!(
        r#"{{"openai-codex":{{"type":"oauth","access":"{TEST_STORE_ACCESS}","refresh":"test-refresh","expires":{},"accountId":"{TEST_STORE_ACCOUNT}"}}}}"#,
        FIXED_NOW_MS + 3_600_000
    ));
    let stub = stub_always(StubReply::ok(fixture("openai_codex_usage.json")));
    let run = run_script(
        "openai_codex.py",
        "openai-codex",
        &agent,
        &stub,
        &with_clock(&[]),
        5_000,
    );
    let _ = envelope(&run);
    let requests = stub.requests();
    assert_eq!(
        requests[0].header("authorization"),
        Some("Bearer test-store-access")
    );
    assert_eq!(
        requests[0].header("chatgpt-account-id"),
        Some("test-store-account")
    );
    assert_no_credential_leak(&run);
}

#[test]
fn codex_invalid_reset_is_shape_changed() {
    let body = fixture_with(
        "openai_codex_usage.json",
        &[
            ("/rate_limit/primary_window/reset_at", Value::from("soon")),
            (
                "/rate_limit/primary_window/reset_after_seconds",
                Value::Null,
            ),
        ],
    );
    let stub = stub_always(StubReply::ok(body));
    let agent = AgentDir::new("codex-reset");
    let run = run_script(
        "openai_codex.py",
        "openai-codex",
        &agent,
        &stub,
        &with_clock(&CODEX),
        5_000,
    );
    assert_error(&run, "shape-changed");
}

#[test]
fn xai_user_then_billing_and_typed_ledgers() {
    let billing = fixture_with(
        "xai_billing.json",
        &[
            ("/config/currentPeriod/end", Value::from(RESET_3D22H_ISO)),
            (
                "/config/currentPeriod/start",
                Value::from("2027-01-12T06:00:00Z"),
            ),
        ],
    );
    let stub = stub_sequence(vec![
        StubReply::ok(fixture("xai_user.json")),
        StubReply::ok(billing),
    ]);
    let agent = AgentDir::new("xai");
    let run = run_script("xai.py", "xai", &agent, &stub, &with_clock(&XAI), 5_000);
    let value = envelope(&run);
    assert_eq!(value["plan"], "SuperGrok");
    assert_eq!(
        value["displayText"],
        "7d 42.5%(~3d22h) / Grok Build 61.2%(~3d22h)"
    );
    let windows = value["windows"].as_array().expect("windows");
    assert_eq!(windows.len(), 2);
    assert_eq!(windows[0]["id"], "included_allowance");
    assert_eq!(windows[0]["label"], "7d");
    assert_eq!(windows[0]["usedPercent"], 42.5);
    assert_eq!(windows[0]["remainingPercent"], 57.5);
    assert_eq!(windows[0]["resetAt"], RESET_3D22H_ISO);
    assert_eq!(windows[0]["durationSeconds"], 604_800);
    // The Grok Build product percentage is kept as its own window; 42.5 and
    // 61.2 are never merged, summed or averaged.
    assert_eq!(windows[1]["id"], "product_grok_build");
    assert_eq!(windows[1]["label"], "Grok Build");
    assert_eq!(windows[1]["usedPercent"], 61.2);
    assert_eq!(windows[1]["remainingPercent"], 38.8);

    let ledgers = value["ledgers"].as_array().expect("ledgers");
    assert_eq!(ledgers.len(), 2);
    assert_eq!(ledgers[0]["id"], "on_demand");
    assert_eq!(ledgers[0]["label"], "On-demand");
    assert_eq!(ledgers[0]["kind"], "on-demand");
    assert_eq!(ledgers[0]["unit"], "USD");
    assert_eq!(ledgers[0]["currency"], "USD");
    assert_eq!(ledgers[0]["total"], 50.0);
    assert_eq!(ledgers[0]["used"], 3.0);
    assert_eq!(ledgers[0]["remaining"], 47.0);
    assert_eq!(ledgers[1]["id"], "prepaid");
    assert_eq!(ledgers[1]["label"], "Prepaid");
    assert_eq!(ledgers[1]["kind"], "prepaid");
    assert_eq!(ledgers[1]["remaining"], 12.5);

    assert_no_credential_leak(&run);
    let requests = stub.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].path, "/v1/user?include=subscription");
    assert_eq!(requests[1].path, "/v1/billing?format=credits");
    for request in &requests {
        assert_eq!(request.method, "GET");
        assert_eq!(request.header("x-xai-token-auth"), Some("xai-grok-cli"));
        assert!(request.header("x-grok-client-version").is_some());
        assert_eq!(
            request.header("x-grok-client-identifier"),
            Some("grok-shell")
        );
        assert_eq!(
            request.header("authorization"),
            Some("Bearer test-oauth-access")
        );
    }
    assert_eq!(
        requests[0].header("x-userid"),
        None,
        "the user request must not carry x-userid"
    );
    assert_eq!(requests[1].header("x-userid"), Some(TEST_USER_ID));
    assert_read_only(&requests);
}

#[test]
fn xai_plan_falls_back_to_the_billing_response() {
    let user = fixture_with("xai_user.json", &[("/subscriptionTier", Value::Null)]);
    let stub = stub_sequence(vec![
        StubReply::ok(user),
        StubReply::ok(fixture("xai_billing.json")),
    ]);
    let agent = AgentDir::new("xai-plan");
    let run = run_script("xai.py", "xai", &agent, &stub, &with_clock(&XAI), 5_000);
    let value = envelope(&run);
    assert_eq!(value["plan"], "SuperGrok");
}

#[test]
fn xai_user_failure_stops_before_billing() {
    let stub = stub_sequence(vec![
        StubReply::Status(500, "{}".to_owned()),
        StubReply::ok(fixture("xai_billing.json")),
    ]);
    let agent = AgentDir::new("xai-user-fail");
    let run = run_script("xai.py", "xai", &agent, &stub, &with_clock(&XAI), 5_000);
    assert_error(&run, "transport");
    let requests = stub.requests();
    assert_eq!(
        requests.len(),
        1,
        "billing must not run after a user failure"
    );
    assert_eq!(requests[0].path, "/v1/user?include=subscription");
    assert_eq!(requests[0].header("x-userid"), None);
}

#[test]
fn xai_missing_user_id_is_shape_changed_without_billing() {
    let user = fixture_with("xai_user.json", &[("/userId", Value::Null)]);
    let stub = stub_sequence(vec![
        StubReply::ok(user),
        StubReply::ok(fixture("xai_billing.json")),
    ]);
    let agent = AgentDir::new("xai-no-user");
    let run = run_script("xai.py", "xai", &agent, &stub, &with_clock(&XAI), 5_000);
    assert_error(&run, "shape-changed");
    assert_eq!(stub.requests().len(), 1);
}

#[test]
fn xai_product_only_response_never_claims_an_overall_percent() {
    let billing = fixture_with(
        "xai_billing.json",
        &[
            ("/config/creditUsagePercent", Value::Null),
            ("/config/currentPeriod", Value::Null),
        ],
    );
    let stub = stub_sequence(vec![
        StubReply::ok(fixture("xai_user.json")),
        StubReply::ok(billing),
    ]);
    let agent = AgentDir::new("xai-product-only");
    let run = run_script("xai.py", "xai", &agent, &stub, &with_clock(&XAI), 5_000);
    let value = envelope(&run);
    let windows = value["windows"].as_array().expect("windows");
    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0]["id"], "product_grok_build");
    assert_eq!(windows[0]["usedPercent"], 61.2);
    assert_eq!(value["displayText"], "Grok Build 61.2%");
}

#[test]
fn xai_omitted_percent_with_complete_period_is_the_proven_zero() {
    let billing = fixture_with(
        "xai_billing.json",
        &[("/config/creditUsagePercent", Value::Null)],
    );
    let stub = stub_sequence(vec![
        StubReply::ok(fixture("xai_user.json")),
        StubReply::ok(billing),
    ]);
    let agent = AgentDir::new("xai-zero");
    let run = run_script("xai.py", "xai", &agent, &stub, &with_clock(&XAI), 5_000);
    let value = envelope(&run);
    let windows = value["windows"].as_array().expect("windows");
    assert_eq!(windows[0]["id"], "included_allowance");
    assert_eq!(windows[0]["usedPercent"], 0.0);
    assert_eq!(windows[1]["id"], "product_grok_build");
}

#[test]
fn xai_missing_period_and_products_is_shape_changed() {
    let billing = fixture_with(
        "xai_billing.json",
        &[
            ("/config/creditUsagePercent", Value::Null),
            ("/config/currentPeriod", Value::Null),
            ("/config/productUsage", Value::Array(Vec::new())),
        ],
    );
    let stub = stub_sequence(vec![
        StubReply::ok(fixture("xai_user.json")),
        StubReply::ok(billing),
    ]);
    let agent = AgentDir::new("xai-empty");
    let run = run_script("xai.py", "xai", &agent, &stub, &with_clock(&XAI), 5_000);
    assert_error(&run, "shape-changed");
}

#[test]
fn xai_invalid_period_reset_is_shape_changed() {
    let billing = fixture_with(
        "xai_billing.json",
        &[("/config/currentPeriod/end", Value::from("next week"))],
    );
    let stub = stub_sequence(vec![
        StubReply::ok(fixture("xai_user.json")),
        StubReply::ok(billing),
    ]);
    let agent = AgentDir::new("xai-reset");
    let run = run_script("xai.py", "xai", &agent, &stub, &with_clock(&XAI), 5_000);
    assert_error(&run, "shape-changed");
}

#[test]
fn xai_overall_without_a_recognized_period_is_shape_changed() {
    // The overall percentage is only labelled/reset by a recognized
    // weekly/monthly currentPeriod; missing or unknown types fail closed.
    for update in [
        ("/config/currentPeriod", Value::Null),
        (
            "/config/currentPeriod/type",
            Value::from("USAGE_PERIOD_TYPE_UNSPECIFIED"),
        ),
    ] {
        let billing = fixture_with("xai_billing.json", &[update]);
        let stub = stub_sequence(vec![
            StubReply::ok(fixture("xai_user.json")),
            StubReply::ok(billing),
        ]);
        let agent = AgentDir::new("xai-period");
        let run = run_script("xai.py", "xai", &agent, &stub, &with_clock(&XAI), 5_000);
        assert_error(&run, "shape-changed");
        assert_eq!(stub.requests().len(), 2, "user then billing");
    }
}

#[test]
fn xai_invalid_percent_is_shape_changed() {
    let billing = fixture_with(
        "xai_billing.json",
        &[("/config/creditUsagePercent", Value::from(142.5))],
    );
    let stub = stub_sequence(vec![
        StubReply::ok(fixture("xai_user.json")),
        StubReply::ok(billing),
    ]);
    let agent = AgentDir::new("xai-percent");
    let run = run_script("xai.py", "xai", &agent, &stub, &with_clock(&XAI), 5_000);
    assert_error(&run, "shape-changed");
}

// ---------------------------------------------------------------------------
// Common failure matrix (all eight providers)
// ---------------------------------------------------------------------------

fn failure_matrix(script: &str, provider: &str, env: &[(&str, &str)]) {
    let cases = [
        (StubReply::Status(401, "{}".to_owned()), "unauthorized"),
        (StubReply::Status(403, "{}".to_owned()), "unauthorized"),
        (StubReply::Status(429, "{}".to_owned()), "rate-limited"),
        (StubReply::Status(500, "{}".to_owned()), "transport"),
        (StubReply::ok("not json"), "shape-changed"),
        (StubReply::oversized(), "shape-changed"),
        (StubReply::redirect(), "transport"),
        (StubReply::Hang, "timeout"),
    ];
    for (reply, expected) in cases {
        let stub = stub_always(reply);
        let agent = AgentDir::new("matrix");
        let run = run_script(script, provider, &agent, &stub, env, 400);
        assert_error(&run, expected);
        assert_no_credential_leak(&run);
        let requests = stub.requests();
        assert_read_only(&requests);
        if script != "glm_coding_plan.py" && script != "kimi_code.py" {
            // GLM/Kimi legitimately retry the two documented auth header
            // shapes; every other script sends exactly one request.
            assert_eq!(requests.len(), 1, "{script}: {expected}");
        }
    }

    // Missing credential: classified auth-unavailable and zero requests.
    let stub = stub_always(StubReply::ok("{}"));
    let agent = AgentDir::new("matrix-no-key");
    let run = run_script(script, provider, &agent, &stub, &with_clock(&[]), 5_000);
    assert_error(&run, "auth-unavailable");
    assert_no_credential_leak(&run);
    assert!(
        stub.requests().is_empty(),
        "{script}: no request without a key"
    );
}

#[test]
fn rpi_offline_short_circuits_every_script_before_credentials_or_requests() {
    // The host is the first offline boundary; the scripts keep a defensive
    // check so a direct invocation cannot reach the network either.
    for (script, provider) in [
        ("deepseek.py", "deepseek"),
        ("glm_coding_plan.py", "glm-coding-plan"),
        ("minimax_token_plan.py", "minimax-token-plan"),
        ("kimi_code.py", "kimi-code"),
        ("opencode_go.py", "opencode-go"),
        ("anthropic.py", "anthropic"),
        ("openai_codex.py", "openai-codex"),
        ("xai.py", "xai"),
    ] {
        let stub = stub_always(StubReply::ok("{}"));
        let agent = AgentDir::new("offline");
        let run = run_script(
            script,
            provider,
            &agent,
            &stub,
            &[("RPI_OFFLINE", "1")],
            5_000,
        );
        assert_error(&run, "offline");
        assert!(
            stub.requests().is_empty(),
            "{script} must not request anything in offline mode"
        );
        assert_no_credential_leak(&run);
    }
}

#[test]
fn deepseek_failure_matrix() {
    failure_matrix("deepseek.py", "deepseek", &DEEPSEEK);
}

#[test]
fn glm_failure_matrix() {
    failure_matrix("glm_coding_plan.py", "glm-coding-plan", &GLM);
}

#[test]
fn minimax_failure_matrix() {
    failure_matrix("minimax_token_plan.py", "minimax-token-plan", &MINIMAX);
}

#[test]
fn kimi_failure_matrix() {
    failure_matrix("kimi_code.py", "kimi-code", &KIMI);
}

#[test]
fn opencode_failure_matrix() {
    failure_matrix("opencode_go.py", "opencode-go", &OPENCODE);
}

#[test]
fn anthropic_failure_matrix() {
    failure_matrix("anthropic.py", "anthropic", &ANTHROPIC);
}

#[test]
fn codex_failure_matrix() {
    failure_matrix("openai_codex.py", "openai-codex", &CODEX);
}

#[test]
fn xai_failure_matrix() {
    // The user request is first, so a failing stub still proves the
    // classified transport/auth path without ever reaching billing.
    failure_matrix("xai.py", "xai", &XAI);
}

// ---------------------------------------------------------------------------
// Credential resolution and endpoint policy
// ---------------------------------------------------------------------------

/// An `auth.json` api_key stored as the braced form `${VAR}` resolves through
/// the environment exactly like `$VAR`.
#[test]
fn auth_store_braced_env_references_resolve() {
    for (script, provider, auth_id) in [
        ("deepseek.py", "deepseek", "deepseek"),
        ("glm_coding_plan.py", "glm-coding-plan", "zai"),
        ("kimi_code.py", "kimi-code", "kimi-coding"),
        ("minimax_token_plan.py", "minimax-token-plan", "minimax"),
        ("opencode_go.py", "opencode-go", "opencode-go"),
    ] {
        let stub = stub_always(StubReply::Status(401, "{}".to_owned()));
        let agent = AgentDir::new(&format!("auth-{auth_id}"));
        agent.write_auth(&format!(
            r#"{{"{auth_id}": {{"type": "api_key", "key": "${{CONTRACT_AUTH_KEY}}"}}}}"#
        ));
        let run = run_script(
            script,
            provider,
            &agent,
            &stub,
            &with_clock(&[("CONTRACT_AUTH_KEY", "test-resolved-secret")]),
            5_000,
        );
        let requests = stub.requests();
        assert!(
            !requests.is_empty(),
            "{script}: the script must attempt a request"
        );
        assert!(
            requests.iter().any(|request| request
                .headers
                .iter()
                .any(|(_, value)| value.ends_with("test-resolved-secret"))),
            "{script}: the braced env reference must resolve (status {:?})",
            run.status
        );
        assert_no_credential_leak(&run);
    }
}

/// A hostile stdin `baseUrl` never changes the endpoint: every request lands
/// on the fixed path at the loopback override origin.
#[test]
fn context_base_url_never_derives_the_endpoint() {
    let stub = stub_always(StubReply::ok(fixture("deepseek_balance.json")));
    let agent = AgentDir::new("base-url");
    let run = run_script("deepseek.py", "deepseek", &agent, &stub, &DEEPSEEK, 5_000);
    let _ = envelope(&run);
    let requests = stub.requests();
    assert_eq!(requests[0].path, "/user/balance");
    assert!(
        requests.iter().all(|request| request
            .header("host")
            .is_some_and(|host| host.starts_with("127.0.0.1:"))),
        "requests must stay on the loopback override: {requests:?}"
    );
}
