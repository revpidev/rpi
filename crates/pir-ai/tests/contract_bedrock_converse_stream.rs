//! Contract tests for the Bedrock ConverseStream adapter: a scripted local
//! HTTP server stands in for `bedrock-runtime.{region}.amazonaws.com`, serving
//! binary AWS event-stream frames (`application/vnd.amazon.eventstream`)
//! encoded with the same codec the adapter decodes with. Assertions cover
//! both sides of the contract — the request shape (path / SigV4 vs bearer
//! auth / reserved-header whitelist / payload JSON) and the emitted
//! `StreamEvent` sequence — plus the pure resolution helpers
//! (region/endpoint, model-family detection, payload builders).
//!
//! Upstream intents ported: `bedrock-convert-messages.test.ts`,
//! `bedrock-custom-headers.test.ts`, `bedrock-endpoint-resolution.test.ts`,
//! `bedrock-thinking-payload.test.ts` (the endpoint/behavior parts of
//! `bedrock-models.test.ts` land with the catalog in W4).

use std::sync::{Arc, Mutex};

use futures::StreamExt;
use pir_ai::api::bedrock::event_stream::{event_frame, exception_frame};
use pir_ai::api::bedrock::sigv4::{self, SigV4Credentials};
use pir_ai::api::bedrock_converse_stream::{
    filtered_custom_headers, format_bedrock_error, get_standard_bedrock_endpoint_region,
    is_reserved_header, map_thinking_level_to_effort, resolve_bedrock_config, stream,
    supports_adaptive_thinking, supports_native_xhigh_effort, BedrockOptions, BedrockToolChoice,
    EMPTY_TEXT_PLACEHOLDER,
};
use pir_ai::types::{
    ApiKind, AssistantMessage, Context, DoneReason, Message, Model, ProviderEnv, StopReason,
    StreamEvent, StreamOptions, ThinkingLevel, ToolResultMessage, ToolResultRole,
};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

// ---------------------------------------------------------------------------
// Scripted HTTP server (binary-capable, one request per test)
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct CapturedRequest {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl CapturedRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// Serves one scripted `(status, content_type, body)` response on a loopback
/// port. Returns the base URL and a channel delivering the captured request.
async fn serve(
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
) -> (String, mpsc::Receiver<CapturedRequest>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (tx, rx) = mpsc::channel(1);
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let request = read_request(&mut socket).await;
        tx.send(request).await.expect("send captured request");
        let reason = match status {
            200 => "OK",
            400 => "Bad Request",
            _ => "Status",
        };
        let response = format!(
            "HTTP/1.1 {status} {reason}\r\ncontent-type: {content_type}\r\nx-amzn-requestid: req-123\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("write head");
        socket.write_all(&body).await.expect("write body");
    });
    (format!("http://{addr}"), rx)
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> CapturedRequest {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let n = socket.read(&mut chunk).await.expect("read");
        if n == 0 {
            panic!("connection closed before headers complete");
        }
        buffer.extend_from_slice(&chunk[..n]);
        if let Some(pos) = find_subslice(&buffer, b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().expect("request line");
    let mut parts = request_line.split(' ');
    let method = parts.next().expect("method").to_owned();
    let path = parts.next().expect("path").to_owned();
    let mut headers = Vec::new();
    let mut content_length = 0usize;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_lowercase();
        let value = value.trim().to_owned();
        if name == "content-length" {
            content_length = value.parse().expect("content-length");
        }
        headers.push((name, value));
    }
    let mut body = buffer[header_end..].to_vec();
    while body.len() < content_length {
        let n = socket.read(&mut chunk).await.expect("read body");
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    CapturedRequest {
        method,
        path,
        headers,
        body,
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

// ---------------------------------------------------------------------------
// Factories
// ---------------------------------------------------------------------------

fn make_model(id: &str, name: &str, base_url: &str, extra: Value) -> Model {
    let mut value = json!({
        "id": id, "name": name, "api": "bedrock-converse-stream",
        "provider": "amazon-bedrock", "baseUrl": base_url,
        "reasoning": true, "input": ["text", "image"],
        "cost": {"input": 3.0, "output": 15.0, "cacheRead": 0.3, "cacheWrite": 3.75},
        "contextWindow": 200000, "maxTokens": 64000
    });
    value
        .as_object_mut()
        .expect("object")
        .extend(extra.as_object().cloned().unwrap_or_default());
    serde_json::from_value(value).expect("model")
}

fn claude_model(base_url: &str) -> Model {
    make_model(
        "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
        "Claude Sonnet 4.5",
        base_url,
        json!({}),
    )
}

fn user_text(text: &str) -> Message {
    serde_json::from_value(json!({"role": "user", "content": text, "timestamp": 0}))
        .expect("user message")
}

fn context(messages: Vec<Message>) -> Context {
    Context {
        system_prompt: None,
        messages,
        tools: None,
    }
}

fn scoped_env(pairs: &[(&str, &str)]) -> ProviderEnv {
    pairs
        .iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

fn sigv4_options(env: ProviderEnv) -> BedrockOptions {
    BedrockOptions {
        stream: StreamOptions {
            env: Some(env),
            ..StreamOptions::default()
        },
        ..BedrockOptions::default()
    }
}

/// `cacheRetention: "none"` as in the upstream payload-capture tests.
fn no_cache_options() -> BedrockOptions {
    let mut options = sigv4_options(test_env());
    options.stream.cache_retention = Some(pir_ai::types::CacheRetention::None);
    options
}

const TEST_ACCESS_KEY: &str = "AKIDEXAMPLE";
const TEST_SECRET_KEY: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";

fn test_env() -> ProviderEnv {
    scoped_env(&[
        ("AWS_ACCESS_KEY_ID", TEST_ACCESS_KEY),
        ("AWS_SECRET_ACCESS_KEY", TEST_SECRET_KEY),
        ("AWS_REGION", "us-east-1"),
    ])
}

/// The standard minimal successful event stream: assistant start, stop,
/// usage metadata.
fn minimal_stream_body() -> Vec<u8> {
    let mut body = event_frame("messageStart", r#"{"role":"assistant"}"#);
    body.extend_from_slice(&event_frame("messageStop", r#"{"stopReason":"end_turn"}"#));
    body.extend_from_slice(&event_frame(
        "metadata",
        r#"{"usage":{"inputTokens":10,"outputTokens":5,"totalTokens":15}}"#,
    ));
    body
}

async fn drive(
    model: &Model,
    ctx: &Context,
    options: BedrockOptions,
    response: (u16, &'static str, Vec<u8>),
) -> (Vec<StreamEvent>, CapturedRequest) {
    let (base_url, mut rx) = serve(response.0, response.1, response.2).await;
    let mut model = model.clone();
    model.base_url = base_url;
    let events: Vec<StreamEvent> = stream(&model, ctx, options).collect().await;
    // Fail fast (instead of hanging) when the stream errors before any
    // request is sent, e.g. a pre-send payload/credential failure.
    let request = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
        .await
        .expect("timed out waiting for the captured request (stream likely failed pre-send)")
        .expect("captured request");
    (events, request)
}

// ---------------------------------------------------------------------------
// SigV4 signing (deterministic, end-to-end through the mock server)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_sigv4_signature_deterministic_over_the_wire() {
    let model = claude_model("http://unused");
    let options = sigv4_options(test_env());
    let (events, request) = drive(
        &model,
        &context(vec![user_text("hi")]),
        options,
        (
            200,
            "application/vnd.amazon.eventstream",
            minimal_stream_body(),
        ),
    )
    .await;
    assert!(matches!(events.last(), Some(StreamEvent::Done { .. })));

    assert_eq!(request.method, "POST");
    assert_eq!(
        request.path,
        "/model/us.anthropic.claude-sonnet-4-5-20250929-v1%3A0/converse-stream"
    );
    assert_eq!(request.header("content-type"), Some("application/json"));

    let authorization = request.header("authorization").expect("authorization");
    let prefix = "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/";
    assert!(authorization.starts_with(prefix), "{authorization}");
    assert!(
        authorization.contains("/us-east-1/bedrock/aws4_request"),
        "{authorization}"
    );
    assert!(
        authorization.contains("SignedHeaders=content-type;host;x-amz-content-sha256;x-amz-date"),
        "{authorization}"
    );

    // The payload hash header matches the captured body, and the signature
    // recomputes from the captured canonical parts.
    let payload_hash = request
        .header("x-amz-content-sha256")
        .expect("x-amz-content-sha256");
    use sha2::{Digest, Sha256};
    assert_eq!(payload_hash, hex(&Sha256::digest(&request.body)));

    let amz_date = request.header("x-amz-date").expect("x-amz-date");
    let host = request.header("host").expect("host");
    let canonical_request = format!(
        "POST\n/model/us.anthropic.claude-sonnet-4-5-20250929-v1%253A0/converse-stream\n\ncontent-type:application/json\nhost:{host}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{amz_date}\n\ncontent-type;host;x-amz-content-sha256;x-amz-date\n{payload_hash}"
    );
    let scope = format!("{}/us-east-1/bedrock/aws4_request", &amz_date[..8]);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        hex(&Sha256::digest(canonical_request.as_bytes()))
    );
    use hmac::{Hmac, Mac};
    let mut mac =
        <Hmac<Sha256> as Mac>::new_from_slice(format!("AWS4{TEST_SECRET_KEY}").as_bytes())
            .expect("hmac key");
    mac.update(&amz_date.as_bytes()[..8]);
    let key = mac.finalize().into_bytes();
    let mut key = key.to_vec();
    for part in ["us-east-1", "bedrock", "aws4_request"] {
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&key).expect("hmac key");
        mac.update(part.as_bytes());
        key = mac.finalize().into_bytes().to_vec();
    }
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&key).expect("hmac key");
    mac.update(string_to_sign.as_bytes());
    let expected_signature = hex(&mac.finalize().into_bytes());
    assert!(
        authorization.contains(&format!("Signature={expected_signature}")),
        "{authorization}"
    );
}

/// Reference-vector check through the public signer (the AWS docs IAM
/// example) so the wire-level test above has an independent anchor.
#[test]
fn test_sigv4_sign_request_reference_vector() {
    let signed = sigv4::sign_request(
        &sigv4::SigV4Request {
            method: "GET",
            path: "/",
            query: &[
                ("Action".to_owned(), "ListUsers".to_owned()),
                ("Version".to_owned(), "2010-05-08".to_owned()),
            ],
            headers: &[
                (
                    "content-type".to_owned(),
                    "application/x-www-form-urlencoded; charset=utf-8".to_owned(),
                ),
                ("host".to_owned(), "iam.amazonaws.com".to_owned()),
            ],
            payload: b"",
        },
        &SigV4Credentials {
            access_key_id: "AKIDEXAMPLE".to_owned(),
            secret_access_key: TEST_SECRET_KEY.to_owned(),
            session_token: None,
        },
        "us-east-1",
        "iam",
        1_440_938_160,
    );
    let authorization = signed
        .iter()
        .find(|(name, _)| name == "authorization")
        .map(|(_, value)| value.as_str())
        .expect("authorization");
    assert_eq!(
        authorization,
        "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/iam/aws4_request, SignedHeaders=content-type;host;x-amz-content-sha256;x-amz-date, Signature=dd479fa8a80364edf2119ec24bebde66712ee9c9cb2b0d92eb3ab9ccdc0c3947"
    );
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[tokio::test]
async fn test_bearer_token_bypasses_sigv4() {
    let model = claude_model("http://unused");
    let mut options = sigv4_options(test_env());
    options.stream.api_key = Some("bedrock-api-key".to_owned());
    let (events, request) = drive(
        &model,
        &context(vec![user_text("hi")]),
        options,
        (
            200,
            "application/vnd.amazon.eventstream",
            minimal_stream_body(),
        ),
    )
    .await;
    assert!(matches!(events.last(), Some(StreamEvent::Done { .. })));
    assert_eq!(
        request.header("authorization"),
        Some("Bearer bedrock-api-key")
    );
    assert!(request.header("x-amz-date").is_none());
    assert!(request.header("x-amz-content-sha256").is_none());
}

// ---------------------------------------------------------------------------
// Normal stream / thinking / tool-call / error stream
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_normal_text_stream() {
    let mut body = event_frame("messageStart", r#"{"role":"assistant"}"#);
    body.extend_from_slice(&event_frame(
        "contentBlockDelta",
        r#"{"contentBlockIndex":0,"delta":{"text":"Hello"}}"#,
    ));
    body.extend_from_slice(&event_frame(
        "contentBlockDelta",
        r#"{"contentBlockIndex":0,"delta":{"text":" world"}}"#,
    ));
    body.extend_from_slice(&event_frame(
        "contentBlockStop",
        r#"{"contentBlockIndex":0}"#,
    ));
    body.extend_from_slice(&event_frame("messageStop", r#"{"stopReason":"end_turn"}"#));
    body.extend_from_slice(&event_frame(
        "metadata",
        r#"{"usage":{"inputTokens":12,"outputTokens":7,"totalTokens":19,"cacheReadInputTokens":4,"cacheWriteInputTokens":2}}"#,
    ));

    let model = claude_model("http://unused");
    let (events, _request) = drive(
        &model,
        &context(vec![user_text("hi")]),
        sigv4_options(test_env()),
        (200, "application/vnd.amazon.eventstream", body),
    )
    .await;
    let kinds: Vec<&str> = events.iter().map(event_kind).collect();
    assert_eq!(
        kinds,
        vec![
            "start",
            "text_start",
            "text_delta",
            "text_delta",
            "text_end",
            "done"
        ]
    );
    let Some(StreamEvent::Done { reason, message }) = events.last() else {
        panic!("expected done event");
    };
    assert_eq!(*reason, DoneReason::Stop);
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(message.usage.input, 12);
    assert_eq!(message.usage.output, 7);
    assert_eq!(message.usage.cache_read, 4);
    assert_eq!(message.usage.cache_write, 2);
    assert_eq!(message.usage.total_tokens, 19);
    assert!(message.usage.cost.total > 0.0);
    match &message.content[0] {
        pir_ai::types::AssistantContent::Text(text) => assert_eq!(text.text, "Hello world"),
        other => panic!("expected text block, got {other:?}"),
    }
}

#[tokio::test]
async fn test_thinking_stream_with_signature() {
    let mut body = event_frame("messageStart", r#"{"role":"assistant"}"#);
    body.extend_from_slice(&event_frame(
        "contentBlockDelta",
        r#"{"contentBlockIndex":0,"delta":{"reasoningContent":{"text":"let me "}}}"#,
    ));
    body.extend_from_slice(&event_frame(
        "contentBlockDelta",
        r#"{"contentBlockIndex":0,"delta":{"reasoningContent":{"text":"think"}}}"#,
    ));
    body.extend_from_slice(&event_frame(
        "contentBlockDelta",
        r#"{"contentBlockIndex":0,"delta":{"reasoningContent":{"signature":"sig-part-"}}}"#,
    ));
    body.extend_from_slice(&event_frame(
        "contentBlockDelta",
        r#"{"contentBlockIndex":0,"delta":{"reasoningContent":{"signature":"end"}}}"#,
    ));
    body.extend_from_slice(&event_frame(
        "contentBlockStop",
        r#"{"contentBlockIndex":0}"#,
    ));
    body.extend_from_slice(&event_frame(
        "contentBlockDelta",
        r#"{"contentBlockIndex":1,"delta":{"text":"answer"}}"#,
    ));
    body.extend_from_slice(&event_frame(
        "contentBlockStop",
        r#"{"contentBlockIndex":1}"#,
    ));
    body.extend_from_slice(&event_frame("messageStop", r#"{"stopReason":"end_turn"}"#));

    let model = claude_model("http://unused");
    let (events, _request) = drive(
        &model,
        &context(vec![user_text("hi")]),
        sigv4_options(test_env()),
        (200, "application/vnd.amazon.eventstream", body),
    )
    .await;
    let kinds: Vec<&str> = events.iter().map(event_kind).collect();
    assert_eq!(
        kinds,
        vec![
            "start",
            "thinking_start",
            "thinking_delta",
            "thinking_delta",
            "thinking_end",
            "text_start",
            "text_delta",
            "text_end",
            "done"
        ]
    );
    let Some(StreamEvent::Done { message, .. }) = events.last() else {
        panic!("expected done event");
    };
    match &message.content[0] {
        pir_ai::types::AssistantContent::Thinking(thinking) => {
            assert_eq!(thinking.thinking, "let me think");
            assert_eq!(thinking.thinking_signature.as_deref(), Some("sig-part-end"));
        }
        other => panic!("expected thinking block, got {other:?}"),
    }
}

#[tokio::test]
async fn test_tool_call_stream() {
    let mut body = event_frame("messageStart", r#"{"role":"assistant"}"#);
    body.extend_from_slice(&event_frame(
        "contentBlockStart",
        r#"{"contentBlockIndex":0,"start":{"toolUse":{"toolUseId":"toolu_1","name":"bash"}}}"#,
    ));
    body.extend_from_slice(&event_frame(
        "contentBlockDelta",
        r#"{"contentBlockIndex":0,"delta":{"toolUse":{"input":"{\"cmd\":"}}}"#,
    ));
    body.extend_from_slice(&event_frame(
        "contentBlockDelta",
        r#"{"contentBlockIndex":0,"delta":{"toolUse":{"input":"\"ls\"}"}}}"#,
    ));
    body.extend_from_slice(&event_frame(
        "contentBlockStop",
        r#"{"contentBlockIndex":0}"#,
    ));
    body.extend_from_slice(&event_frame("messageStop", r#"{"stopReason":"tool_use"}"#));

    let model = claude_model("http://unused");
    let (events, _request) = drive(
        &model,
        &context(vec![user_text("hi")]),
        sigv4_options(test_env()),
        (200, "application/vnd.amazon.eventstream", body),
    )
    .await;
    let kinds: Vec<&str> = events.iter().map(event_kind).collect();
    assert_eq!(
        kinds,
        vec![
            "start",
            "toolcall_start",
            "toolcall_delta",
            "toolcall_delta",
            "toolcall_end",
            "done"
        ]
    );
    let Some(StreamEvent::Done { reason, message }) = events.last() else {
        panic!("expected done event");
    };
    assert_eq!(*reason, DoneReason::ToolUse);
    let pir_ai::types::AssistantContent::ToolCall(call) = &message.content[0] else {
        panic!("expected tool call block");
    };
    assert_eq!(call.id, "toolu_1");
    assert_eq!(call.name, "bash");
    assert_eq!(Value::Object(call.arguments.clone()), json!({"cmd": "ls"}));
    // The toolcall_end event carries the finalized call.
    let end_event = events
        .iter()
        .find(|event| matches!(event, StreamEvent::ToolCallEnd { .. }))
        .expect("toolcall_end");
    let StreamEvent::ToolCallEnd { tool_call, .. } = end_event else {
        unreachable!()
    };
    assert_eq!(
        Value::Object(tool_call.arguments.clone()),
        json!({"cmd": "ls"})
    );
}

#[tokio::test]
async fn test_error_stream_exception_frame() {
    let mut body = event_frame("messageStart", r#"{"role":"assistant"}"#);
    body.extend_from_slice(&exception_frame(
        "throttlingException",
        r#"{"message":"Rate exceeded"}"#,
    ));

    let model = claude_model("http://unused");
    let (events, _request) = drive(
        &model,
        &context(vec![user_text("hi")]),
        sigv4_options(test_env()),
        (200, "application/vnd.amazon.eventstream", body),
    )
    .await;
    assert_eq!(events.len(), 2); // start + error
    let Some(StreamEvent::Error { error, .. }) = events.last() else {
        panic!("expected error event, got {events:?}");
    };
    assert_eq!(error.stop_reason, StopReason::Error);
    assert_eq!(
        error.error_message,
        Some("Throttling error: Rate exceeded".to_owned())
    );
}

#[tokio::test]
async fn test_error_stream_http_error_with_exception_name() {
    let model = claude_model("http://unused");
    let (events, request) = drive(
        &model,
        &context(vec![user_text("hi")]),
        sigv4_options(test_env()),
        (
            400,
            "application/json",
            br#"{"message":"bad input"}"#.to_vec(),
        ),
    )
    .await;
    let _ = request;
    assert_eq!(events.len(), 1);
    let Some(StreamEvent::Error { error, .. }) = events.last() else {
        panic!("expected error event, got {events:?}");
    };
    // No x-amzn-errortype header from the mock: no exception prefix. The raw
    // body is surfaced with the status (upstream `formatBedrockError`:
    // `status: body` when the extracted `message` field does not carry the
    // raw body — normalizeProviderError picks `$response.body` verbatim).
    assert_eq!(
        error.error_message,
        Some("400: {\"message\":\"bad input\"}".to_owned())
    );
}

#[tokio::test]
async fn test_message_start_user_role_is_error() {
    let body = event_frame("messageStart", r#"{"role":"user"}"#);
    let model = claude_model("http://unused");
    let (events, _request) = drive(
        &model,
        &context(vec![user_text("hi")]),
        sigv4_options(test_env()),
        (200, "application/vnd.amazon.eventstream", body),
    )
    .await;
    let Some(StreamEvent::Error { error, .. }) = events.last() else {
        panic!("expected error event, got {events:?}");
    };
    assert_eq!(
        error.error_message,
        Some("Unexpected assistant message start but got user message start instead".to_owned())
    );
}

fn event_kind(event: &StreamEvent) -> &'static str {
    match event {
        StreamEvent::Start { .. } => "start",
        StreamEvent::TextStart { .. } => "text_start",
        StreamEvent::TextDelta { .. } => "text_delta",
        StreamEvent::TextEnd { .. } => "text_end",
        StreamEvent::ThinkingStart { .. } => "thinking_start",
        StreamEvent::ThinkingDelta { .. } => "thinking_delta",
        StreamEvent::ThinkingEnd { .. } => "thinking_end",
        StreamEvent::ToolCallStart { .. } => "toolcall_start",
        StreamEvent::ToolCallDelta { .. } => "toolcall_delta",
        StreamEvent::ToolCallEnd { .. } => "toolcall_end",
        StreamEvent::Done { .. } => "done",
        StreamEvent::Error { .. } => "error",
    }
}

// ---------------------------------------------------------------------------
// Endpoint / region resolution (bedrock-endpoint-resolution.test.ts intents)
// ---------------------------------------------------------------------------

/// Serializes tests that mutate process env (the ambient `AWS_PROFILE` check
/// deliberately reads the process env only, mirroring upstream).
static ENV_LOCK: Mutex<()> = Mutex::new(());

struct EnvGuard {
    saved: Vec<(&'static str, Option<String>)>,
}

impl EnvGuard {
    fn clear(names: &[&'static str]) -> Self {
        let saved = names
            .iter()
            .map(|name| {
                let value = std::env::var(name).ok();
                std::env::remove_var(name);
                (*name, value)
            })
            .collect();
        Self { saved }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (name, value) in &self.saved {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

#[test]
fn test_standard_endpoint_region_extraction() {
    assert_eq!(
        get_standard_bedrock_endpoint_region("https://bedrock-runtime.eu-central-1.amazonaws.com"),
        Some("eu-central-1".to_owned())
    );
    assert_eq!(
        get_standard_bedrock_endpoint_region(
            "https://bedrock-runtime-fips.us-gov-west-1.amazonaws.com"
        ),
        Some("us-gov-west-1".to_owned())
    );
    assert_eq!(
        get_standard_bedrock_endpoint_region("https://bedrock-runtime.cn-north-1.amazonaws.com.cn"),
        Some("cn-north-1".to_owned())
    );
    assert_eq!(
        get_standard_bedrock_endpoint_region("https://bedrock-vpc.example.com"),
        None
    );
    assert_eq!(get_standard_bedrock_endpoint_region("not a url"), None);
}

#[test]
fn test_resolve_config_region_from_env_endpoint_not_pinned() {
    // "does not pin standard AWS endpoints when AWS_REGION is configured".
    let _lock = ENV_LOCK.lock().expect("env lock");
    let _guard = EnvGuard::clear(&["AWS_REGION", "AWS_DEFAULT_REGION", "AWS_PROFILE"]);
    let model = claude_model("https://bedrock-runtime.us-east-1.amazonaws.com");
    let options = sigv4_options(scoped_env(&[("AWS_REGION", "us-east-2")]));
    let config = resolve_bedrock_config(&model, &options);
    assert_eq!(config.region.as_deref(), Some("us-east-2"));
    assert_eq!(config.endpoint, None);
}

#[test]
fn test_resolve_config_standard_endpoint_derives_region() {
    // "derives region from a built-in EU endpoint when no region or profile
    // is configured": endpoint pinned, region from the hostname.
    let _lock = ENV_LOCK.lock().expect("env lock");
    let _guard = EnvGuard::clear(&["AWS_REGION", "AWS_DEFAULT_REGION", "AWS_PROFILE"]);
    let model = claude_model("https://bedrock-runtime.eu-central-1.amazonaws.com");
    let config = resolve_bedrock_config(&model, &BedrockOptions::default());
    assert_eq!(
        config.endpoint.as_deref(),
        Some("https://bedrock-runtime.eu-central-1.amazonaws.com")
    );
    assert_eq!(config.region.as_deref(), Some("eu-central-1"));
}

#[test]
fn test_resolve_config_explicit_and_scoped_profiles() {
    let _lock = ENV_LOCK.lock().expect("env lock");
    let _guard = EnvGuard::clear(&["AWS_REGION", "AWS_DEFAULT_REGION", "AWS_PROFILE"]);
    let model = claude_model("https://bedrock-runtime.eu-central-1.amazonaws.com");

    // Explicit profile option: endpoint pinned, region from the endpoint.
    let mut options = BedrockOptions {
        profile: Some("bedrock-profile".to_owned()),
        ..BedrockOptions::default()
    };
    let config = resolve_bedrock_config(&model, &options);
    assert_eq!(config.profile.as_deref(), Some("bedrock-profile"));
    assert_eq!(
        config.endpoint.as_deref(),
        Some("https://bedrock-runtime.eu-central-1.amazonaws.com")
    );
    assert_eq!(config.region.as_deref(), Some("eu-central-1"));

    // Scoped AWS_PROFILE (options env) counts as configured, not ambient.
    options.profile = None;
    options.stream.env = Some(scoped_env(&[("AWS_PROFILE", "scoped-bedrock-profile")]));
    let config = resolve_bedrock_config(&model, &options);
    assert_eq!(config.profile.as_deref(), Some("scoped-bedrock-profile"));
    assert!(config.endpoint.is_some());
    assert_eq!(config.region.as_deref(), Some("eu-central-1"));

    // Ambient process AWS_PROFILE: endpoint NOT pinned and region deferred
    // (upstream: SDK default chain; pir falls back to us-east-1 downstream).
    std::env::set_var("AWS_PROFILE", "ambient-bedrock-profile");
    let config = resolve_bedrock_config(&model, &BedrockOptions::default());
    assert_eq!(config.profile.as_deref(), Some("ambient-bedrock-profile"));
    assert_eq!(config.endpoint, None);
    assert_eq!(config.region, None);
    assert_eq!(config.effective_region(), "us-east-1");
    std::env::remove_var("AWS_PROFILE");
}

#[test]
fn test_resolve_config_custom_endpoint_with_region() {
    let _lock = ENV_LOCK.lock().expect("env lock");
    let _guard = EnvGuard::clear(&["AWS_DEFAULT_REGION", "AWS_PROFILE"]);
    let model = claude_model("https://bedrock-vpc.example.com");
    let options = sigv4_options(scoped_env(&[("AWS_REGION", "us-west-2")]));
    let config = resolve_bedrock_config(&model, &options);
    assert_eq!(
        config.endpoint.as_deref(),
        Some("https://bedrock-vpc.example.com")
    );
    assert_eq!(config.region.as_deref(), Some("us-west-2"));
}

#[test]
fn test_resolve_config_arn_region_wins() {
    let _lock = ENV_LOCK.lock().expect("env lock");
    let _guard = EnvGuard::clear(&["AWS_DEFAULT_REGION", "AWS_PROFILE"]);
    let mut model = claude_model("https://bedrock-runtime.us-east-1.amazonaws.com");
    model.id =
        "arn:aws:bedrock:us-west-2:123456789012:application-inference-profile/abc123".to_owned();
    let options = sigv4_options(scoped_env(&[("AWS_REGION", "us-east-1")]));
    let config = resolve_bedrock_config(&model, &options);
    assert_eq!(config.region.as_deref(), Some("us-west-2"));

    // GovCloud inference profile ARN.
    model.id =
        "arn:aws-us-gov:bedrock:us-gov-west-1:123456789012:application-inference-profile/abc123"
            .to_owned();
    let config = resolve_bedrock_config(&model, &options);
    assert_eq!(config.region.as_deref(), Some("us-gov-west-1"));
}

#[test]
fn test_resolve_config_bearer_from_api_key_and_skip_auth() {
    let _lock = ENV_LOCK.lock().expect("env lock");
    let _guard = EnvGuard::clear(&["AWS_REGION", "AWS_DEFAULT_REGION", "AWS_PROFILE"]);
    let model = claude_model("https://bedrock-runtime.us-east-1.amazonaws.com");

    // "uses the generic API key option as a Bedrock bearer token".
    let mut options = sigv4_options(test_env());
    options.stream.api_key = Some("bedrock-api-key".to_owned());
    let config = resolve_bedrock_config(&model, &options);
    assert_eq!(config.bearer_token.as_deref(), Some("bedrock-api-key"));
    assert!(config.credentials.is_none());

    // AWS_BEDROCK_SKIP_AUTH=1: dummy SigV4 credentials, bearer disabled.
    let mut options = sigv4_options(scoped_env(&[("AWS_BEDROCK_SKIP_AUTH", "1")]));
    options.stream.api_key = Some("bedrock-api-key".to_owned());
    let config = resolve_bedrock_config(&model, &options);
    assert_eq!(config.bearer_token, None);
    let credentials = config.credentials.expect("dummy credentials");
    assert_eq!(credentials.access_key_id, "dummy-access-key");
}

// ---------------------------------------------------------------------------
// Reserved-header whitelist (bedrock-custom-headers.test.ts intents)
// ---------------------------------------------------------------------------

#[test]
fn test_is_reserved_header() {
    for reserved in [
        "authorization",
        "Authorization",
        "host",
        "HOST",
        "x-amz-date",
        "X-Amz-Date",
        "x-amz-security-token",
    ] {
        assert!(is_reserved_header(reserved), "{reserved}");
    }
    for allowed in ["x-custom", "x-allowed", "content-type", "x-amzn-trace-id"] {
        assert!(!is_reserved_header(allowed), "{allowed}");
    }
}

#[test]
fn test_filtered_custom_headers_skips_reserved_case_insensitively() {
    let headers: pir_ai::types::ProviderHeaders = [
        ("authorization".to_owned(), Some("evil".to_owned())),
        ("x-amz-date".to_owned(), Some("evil".to_owned())),
        ("x-allowed".to_owned(), Some("ok".to_owned())),
        ("Authorization".to_owned(), Some("evil2".to_owned())),
        ("X-Amz-Date".to_owned(), Some("evil2".to_owned())),
        ("HOST".to_owned(), Some("evil3".to_owned())),
        ("x-dropped".to_owned(), None),
    ]
    .into();
    let filtered = filtered_custom_headers(Some(&headers));
    assert_eq!(filtered, vec![("x-allowed".to_owned(), "ok".to_owned())]);
}

#[tokio::test]
async fn test_custom_headers_applied_reserved_not_overridden() {
    let model = claude_model("http://unused");
    let mut options = sigv4_options(test_env());
    options.stream.headers = Some(
        [
            ("x-custom".to_owned(), Some("v".to_owned())),
            ("authorization".to_owned(), Some("evil".to_owned())),
            ("x-amz-date".to_owned(), Some("evil".to_owned())),
            ("host".to_owned(), Some("evil".to_owned())),
        ]
        .into(),
    );
    let (_events, request) = drive(
        &model,
        &context(vec![user_text("hi")]),
        options,
        (
            200,
            "application/vnd.amazon.eventstream",
            minimal_stream_body(),
        ),
    )
    .await;
    assert_eq!(request.header("x-custom"), Some("v"));
    // Reserved caller values must not have replaced the signed headers.
    assert_ne!(request.header("authorization"), Some("evil"));
    assert!(request
        .header("authorization")
        .expect("authorization")
        .starts_with("AWS4-HMAC-SHA256 "),);
    assert_ne!(request.header("x-amz-date"), Some("evil"));
    assert_ne!(request.header("host"), Some("evil"));
    // The custom header participated in the signature (build step).
    assert!(
        request
            .header("authorization")
            .expect("authorization")
            .contains("x-custom"),
        "custom header must be signed"
    );
}

// ---------------------------------------------------------------------------
// EMPTY_TEXT_PLACEHOLDER (bedrock-convert-messages.test.ts intents)
// ---------------------------------------------------------------------------

/// Payload capture through `on_payload`, with the mock server completing the
/// stream afterwards (the upstream tests abort after capture).
async fn capture_payload(
    model: &Model,
    ctx: &Context,
    mut options: BedrockOptions,
) -> (Value, CapturedRequest) {
    let captured = Arc::new(Mutex::new(Value::Null));
    let slot = captured.clone();
    options.stream.on_payload = Some(Arc::new(move |payload, _model| {
        let slot = slot.clone();
        Box::pin(async move {
            *slot.lock().expect("payload slot") = payload;
            None
        })
    }));
    let (_events, request) = drive(
        model,
        ctx,
        options,
        (
            200,
            "application/vnd.amazon.eventstream",
            minimal_stream_body(),
        ),
    )
    .await;
    let payload = captured.lock().expect("payload slot").clone();
    (payload, request)
}

#[tokio::test]
async fn test_convert_messages_empty_text_placeholder() {
    let model = claude_model("http://unused");

    // Blank user string content -> placeholder.
    let (payload, _) =
        capture_payload(&model, &context(vec![user_text("   ")]), no_cache_options()).await;
    assert_eq!(
        payload["messages"][0]["content"],
        json!([{"text": EMPTY_TEXT_PLACEHOLDER}])
    );

    // User blocks emptied after filtering -> placeholder.
    let blank_blocks: Message = serde_json::from_value(
        json!({"role": "user", "content": [{"type": "text", "text": ""}], "timestamp": 0}),
    )
    .expect("user blocks");
    let (payload, _) =
        capture_payload(&model, &context(vec![blank_blocks]), no_cache_options()).await;
    assert_eq!(
        payload["messages"][0]["content"],
        json!([{"text": EMPTY_TEXT_PLACEHOLDER}])
    );

    // Blank user text blocks drop out when other content remains.
    let mixed: Message = serde_json::from_value(
        json!({"role": "user", "content": [{"type": "text", "text": ""}, {"type": "text", "text": "hello"}], "timestamp": 0}),
    )
    .expect("user blocks");
    let (payload, _) = capture_payload(&model, &context(vec![mixed]), no_cache_options()).await;
    assert_eq!(
        payload["messages"][0]["content"],
        json!([{"text": "hello"}])
    );

    // Blank tool result content -> placeholder inside the toolResult block.
    let tool_result = Message::ToolResult(ToolResultMessage {
        role: ToolResultRole::ToolResult,
        tool_call_id: "tool-1".to_owned(),
        tool_name: "tool".to_owned(),
        content: vec![pir_ai::types::ToolResultContent::Text(
            pir_ai::types::TextContent {
                text: String::new(),
                text_signature: None,
            },
        )],
        details: None,
        usage: None,
        added_tool_names: None,
        is_error: false,
        timestamp: 0,
    });
    let assistant = Message::Assistant(AssistantMessage {
        role: pir_ai::types::AssistantRole::Assistant,
        content: vec![pir_ai::types::AssistantContent::ToolCall(
            pir_ai::types::ToolCall {
                id: "tool-1".to_owned(),
                name: "tool".to_owned(),
                arguments: serde_json::Map::new(),
                thought_signature: None,
            },
        )],
        api: ApiKind::from(ApiKind::BEDROCK_CONVERSE_STREAM),
        provider: "amazon-bedrock".to_owned(),
        model: model.id.clone(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        usage: Default::default(),
        stop_reason: StopReason::ToolUse,
        error_message: None,
        timestamp: 0,
    });
    let (payload, _) = capture_payload(
        &model,
        &context(vec![assistant, tool_result]),
        no_cache_options(),
    )
    .await;
    assert_eq!(
        payload["messages"][1]["content"][0]["toolResult"]["content"],
        json!([{"text": EMPTY_TEXT_PLACEHOLDER}])
    );
}

#[tokio::test]
async fn test_convert_messages_assistant_blank_and_thinking() {
    let model = claude_model("http://unused");
    // Assistant text emptied by sanitization drops the whole message;
    // signed thinking replays as reasoningContent, unsigned as plain text.
    let assistant = Message::Assistant(AssistantMessage {
        role: pir_ai::types::AssistantRole::Assistant,
        content: vec![
            pir_ai::types::AssistantContent::Thinking(pir_ai::types::ThinkingContent {
                thinking: "deep thought".to_owned(),
                thinking_signature: Some("sig".to_owned()),
                redacted: None,
            }),
            pir_ai::types::AssistantContent::Thinking(pir_ai::types::ThinkingContent {
                thinking: "unsigned thought".to_owned(),
                thinking_signature: None,
                redacted: None,
            }),
            pir_ai::types::AssistantContent::Text(pir_ai::types::TextContent {
                text: "   ".to_owned(),
                text_signature: None,
            }),
        ],
        api: ApiKind::from(ApiKind::BEDROCK_CONVERSE_STREAM),
        provider: "amazon-bedrock".to_owned(),
        model: model.id.clone(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        usage: Default::default(),
        stop_reason: StopReason::Stop,
        error_message: None,
        timestamp: 0,
    });
    let (payload, _) =
        capture_payload(&model, &context(vec![assistant]), sigv4_options(test_env())).await;
    assert_eq!(
        payload["messages"][0]["content"],
        json!([
            {"reasoningContent": {"reasoningText": {"text": "deep thought", "signature": "sig"}}},
            {"text": "unsigned thought"},
        ])
    );
}

#[tokio::test]
async fn test_convert_messages_groups_tool_results_and_cache_points() {
    let model = claude_model("http://unused");
    let mut ctx = context(vec![user_text("hi")]);
    ctx.system_prompt = Some("be nice".to_owned());
    let (payload, _) = capture_payload(&model, &ctx, sigv4_options(test_env())).await;
    // Default retention (short): system prompt and last user message get
    // cache points without ttl.
    assert_eq!(
        payload["system"],
        json!([{"text": "be nice"}, {"cachePoint": {"type": "default"}}])
    );
    let messages = payload["messages"].as_array().expect("messages");
    let last = messages.last().expect("last message");
    assert_eq!(last["role"], json!("user"));
    assert_eq!(
        last["content"].as_array().expect("content").last(),
        Some(&json!({"cachePoint": {"type": "default"}}))
    );
}

#[tokio::test]
async fn test_cache_point_long_retention_ttl_1h() {
    let model = claude_model("http://unused");
    let mut ctx = context(vec![user_text("hi")]);
    ctx.system_prompt = Some("be nice".to_owned());
    let mut options = sigv4_options(test_env());
    options.stream.cache_retention = Some(pir_ai::types::CacheRetention::Long);
    let (payload, _) = capture_payload(&model, &ctx, options).await;
    assert_eq!(
        payload["system"][1],
        json!({"cachePoint": {"type": "default", "ttl": "1h"}})
    );
    let messages = payload["messages"].as_array().expect("messages");
    assert_eq!(
        messages.last().expect("last")["content"]
            .as_array()
            .expect("c")
            .last(),
        Some(&json!({"cachePoint": {"type": "default", "ttl": "1h"}}))
    );

    // Retention none: no cache points anywhere.
    let mut options = sigv4_options(test_env());
    options.stream.cache_retention = Some(pir_ai::types::CacheRetention::None);
    let (payload, _) = capture_payload(&model, &ctx, options).await;
    assert_eq!(payload["system"], json!([{"text": "be nice"}]));
}

#[tokio::test]
async fn test_tool_config_strict_gating() {
    // "gates native strict tool use by model capability".
    let tools: Vec<pir_ai::types::Tool> = serde_json::from_value(json!([{
        "name": "lookup",
        "description": "Look up a value",
        "parameters": {"type": "object", "properties": {"value": {"type": "string"}}},
        "constrainedSampling": {"type": "json_schema", "strict": "require"},
    }]))
    .expect("tools");
    let mut ctx = context(vec![user_text("use the tool")]);
    ctx.tools = Some(tools);

    let strict_model = make_model(
        "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
        "Claude Sonnet 4.5",
        "http://unused",
        json!({"compat": {"supportsStrictMode": true}}),
    );
    let (payload, _) = capture_payload(&strict_model, &ctx, no_cache_options()).await;
    assert_eq!(
        payload["toolConfig"]["tools"][0]["toolSpec"]["strict"],
        json!(true)
    );

    let nova_model = make_model(
        "amazon.nova-lite-v1:0",
        "Nova Lite",
        "http://unused",
        json!({"reasoning": false}),
    );
    // Upstream switches the tool to strict:"prefer" for the capability-gated
    // case ("require" on an incapable model is an error by design).
    let prefer_tools: Vec<pir_ai::types::Tool> = serde_json::from_value(json!([{
        "name": "lookup",
        "description": "Look up a value",
        "parameters": {"type": "object", "properties": {"value": {"type": "string"}}},
        "constrainedSampling": {"type": "json_schema", "strict": "prefer"},
    }]))
    .expect("tools");
    let mut prefer_ctx = context(vec![user_text("use the tool")]);
    prefer_ctx.tools = Some(prefer_tools);
    let (payload, _) = capture_payload(&nova_model, &prefer_ctx, no_cache_options()).await;
    assert!(payload["toolConfig"]["tools"][0]["toolSpec"]
        .get("strict")
        .is_none());

    // toolChoice mapping.
    let mut options = no_cache_options();
    options.tool_choice = Some(BedrockToolChoice::Any);
    let (payload, _) = capture_payload(&strict_model, &ctx, options).await;
    assert_eq!(payload["toolConfig"]["toolChoice"], json!({"any": {}}));

    let mut options = no_cache_options();
    options.tool_choice = Some(BedrockToolChoice::None);
    let (payload, _) = capture_payload(&strict_model, &ctx, options).await;
    assert!(payload.get("toolConfig").is_none());
}

// ---------------------------------------------------------------------------
// Adaptive thinking model families (bedrock-thinking-payload.test.ts intents)
// ---------------------------------------------------------------------------

#[test]
fn test_supports_adaptive_thinking_family_matrix() {
    for (id, name) in [
        ("global.anthropic.claude-opus-4-6-v1", "Claude Opus 4.6"),
        ("global.anthropic.claude-opus-4-7-v1", "Claude Opus 4.7"),
        ("global.anthropic.claude-opus-4-8-v1", "Claude Opus 4.8"),
        ("global.anthropic.claude-opus-5", "Claude Opus 5"),
        ("global.anthropic.claude-sonnet-4-6", "Claude Sonnet 4.6"),
        ("global.anthropic.claude-sonnet-5", "Claude Sonnet 5"),
        ("global.anthropic.claude-fable-5", "Claude Fable 5"),
    ] {
        assert!(supports_adaptive_thinking(id, name), "{id}");
    }
    for (id, name) in [
        (
            "us.anthropic.claude-opus-4-5-20251101-v1:0",
            "Claude Opus 4.5",
        ),
        (
            "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
            "Claude Sonnet 4.5",
        ),
        (
            "us.anthropic.claude-haiku-4-5-20251001-v1:0",
            "Claude Haiku 4.5",
        ),
        ("amazon.nova-lite-v1:0", "Nova Lite"),
    ] {
        assert!(!supports_adaptive_thinking(id, name), "{id}");
    }
    // Application inference profile ARN: the name carries the family.
    assert!(supports_adaptive_thinking(
        "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/my-profile",
        "Claude Opus 4.6"
    ));
}

#[test]
fn test_supports_native_xhigh_effort_subset() {
    let xhigh = |id: &str, name: &str| {
        let model = make_model(id, name, "http://unused", json!({}));
        supports_native_xhigh_effort(&model)
    };
    // The adaptive families minus opus-4-6 and sonnet-4-6.
    assert!(xhigh("global.anthropic.claude-opus-4-7-v1", "Opus 4.7"));
    assert!(xhigh("global.anthropic.claude-opus-4-8-v1", "Opus 4.8"));
    assert!(xhigh("global.anthropic.claude-opus-5", "Opus 5"));
    assert!(xhigh("global.anthropic.claude-sonnet-5", "Sonnet 5"));
    assert!(xhigh("global.anthropic.claude-fable-5", "Fable 5"));
    assert!(!xhigh("global.anthropic.claude-opus-4-6-v1", "Opus 4.6"));
    assert!(!xhigh("global.anthropic.claude-sonnet-4-6", "Sonnet 4.6"));
}

#[test]
fn test_map_thinking_level_to_effort() {
    let opus48 = make_model(
        "global.anthropic.claude-opus-4-8-v1",
        "Claude Opus 4.8 (Global)",
        "http://unused",
        json!({}),
    );
    assert_eq!(
        map_thinking_level_to_effort(&opus48, Some(ThinkingLevel::Xhigh)),
        "xhigh"
    );
    assert_eq!(
        map_thinking_level_to_effort(&opus48, Some(ThinkingLevel::High)),
        "high"
    );
    assert_eq!(
        map_thinking_level_to_effort(&opus48, Some(ThinkingLevel::Minimal)),
        "low"
    );
    // opus-4-6 has no native xhigh: falls to the default ladder ("high").
    let opus46 = make_model(
        "global.anthropic.claude-opus-4-6-v1",
        "Claude Opus 4.6",
        "http://unused",
        json!({}),
    );
    assert_eq!(
        map_thinking_level_to_effort(&opus46, Some(ThinkingLevel::Xhigh)),
        "high"
    );
    // thinkingLevelMap values pass through verbatim.
    let mapped = make_model(
        "global.anthropic.claude-opus-4-8-v1",
        "Claude Opus 4.8",
        "http://unused",
        json!({"thinkingLevelMap": {"high": "max"}}),
    );
    assert_eq!(
        map_thinking_level_to_effort(&mapped, Some(ThinkingLevel::High)),
        "max"
    );
}

#[tokio::test]
async fn test_thinking_payload_adaptive_and_budget() {
    let ctx = context(vec![user_text("Hello")]);

    // Adaptive: Opus 4.8 -> effort output_config, no anthropic_beta.
    let opus48 = make_model(
        "global.anthropic.claude-opus-4-8-v1",
        "Claude Opus 4.8 (Global)",
        "http://unused",
        json!({}),
    );
    let mut options = sigv4_options(test_env());
    options.reasoning = Some(ThinkingLevel::High);
    let (payload, _) = capture_payload(&opus48, &ctx, options).await;
    assert_eq!(
        payload["additionalModelRequestFields"]["thinking"],
        json!({"type": "adaptive", "display": "summarized"})
    );
    assert_eq!(
        payload["additionalModelRequestFields"]["output_config"],
        json!({"effort": "high"})
    );
    assert!(payload["additionalModelRequestFields"]
        .get("anthropic_beta")
        .is_none());

    // Adaptive + xhigh on Fable 5.
    let fable = make_model(
        "global.anthropic.claude-fable-5",
        "Claude Fable 5",
        "http://unused",
        json!({}),
    );
    let mut options = sigv4_options(test_env());
    options.reasoning = Some(ThinkingLevel::Xhigh);
    let (payload, _) = capture_payload(&fable, &ctx, options).await;
    assert_eq!(
        payload["additionalModelRequestFields"]["output_config"],
        json!({"effort": "xhigh"})
    );

    // Budget-based: Sonnet 4.5 -> enabled + budget_tokens + interleaved beta.
    let sonnet45 = claude_model("http://unused");
    let mut options = sigv4_options(test_env());
    options.reasoning = Some(ThinkingLevel::High);
    let (payload, _) = capture_payload(&sonnet45, &ctx, options).await;
    assert_eq!(
        payload["additionalModelRequestFields"]["thinking"],
        json!({"type": "enabled", "budget_tokens": 16384, "display": "summarized"})
    );
    assert_eq!(
        payload["additionalModelRequestFields"]["anthropic_beta"],
        json!(["interleaved-thinking-2025-05-14"])
    );

    // Interleaved thinking disabled by option.
    let mut options = sigv4_options(test_env());
    options.reasoning = Some(ThinkingLevel::High);
    options.interleaved_thinking = Some(false);
    let (payload, _) = capture_payload(&sonnet45, &ctx, options).await;
    assert!(payload["additionalModelRequestFields"]
        .get("anthropic_beta")
        .is_none());

    // Non-Claude model: no additionalModelRequestFields at all.
    let nova = make_model(
        "amazon.nova-lite-v1:0",
        "Nova Lite",
        "http://unused",
        json!({"reasoning": false}),
    );
    let mut options = sigv4_options(test_env());
    options.reasoning = Some(ThinkingLevel::High);
    let (payload, _) = capture_payload(&nova, &ctx, options).await;
    assert!(payload.get("additionalModelRequestFields").is_none());
}

#[tokio::test]
async fn test_thinking_payload_govcloud_omits_display() {
    let ctx = context(vec![user_text("Hello")]);

    // Non-adaptive GovCloud model id.
    let gov = make_model(
        "us-gov.anthropic.claude-sonnet-4-5-20250929-v1:0",
        "Claude Sonnet 4.5 (GovCloud)",
        "http://unused",
        json!({}),
    );
    let mut options = sigv4_options(test_env());
    options.reasoning = Some(ThinkingLevel::High);
    let (payload, _) = capture_payload(&gov, &ctx, options).await;
    assert_eq!(
        payload["additionalModelRequestFields"]["thinking"],
        json!({"type": "enabled", "budget_tokens": 16384})
    );

    // Adaptive model in a GovCloud region (option).
    let opus48 = make_model(
        "global.anthropic.claude-opus-4-8-v1",
        "Claude Opus 4.8 (Global)",
        "http://unused",
        json!({}),
    );
    let mut options = sigv4_options(test_env());
    options.reasoning = Some(ThinkingLevel::High);
    options.region = Some("us-gov-west-1".to_owned());
    let (payload, _) = capture_payload(&opus48, &ctx, options).await;
    assert_eq!(
        payload["additionalModelRequestFields"]["thinking"],
        json!({"type": "adaptive"})
    );
}

#[tokio::test]
async fn test_stream_simple_budget_path_payload() {
    use pir_ai::api::bedrock_converse_stream::stream_simple;
    let sonnet45 = claude_model("http://unused");
    let ctx = context(vec![user_text("Hello")]);
    let captured = Arc::new(Mutex::new(Value::Null));
    let slot = captured.clone();
    let simple = pir_ai::types::SimpleStreamOptions {
        stream: StreamOptions {
            env: Some(test_env()),
            on_payload: Some(Arc::new(move |payload, _model| {
                let slot = slot.clone();
                Box::pin(async move {
                    *slot.lock().expect("payload slot") = payload;
                    None
                })
            })),
            ..StreamOptions::default()
        },
        reasoning: Some(ThinkingLevel::High),
        thinking_budgets: None,
    };
    let (base_url, mut rx) = serve(
        200,
        "application/vnd.amazon.eventstream",
        minimal_stream_body(),
    )
    .await;
    let mut model = sonnet45.clone();
    model.base_url = base_url;
    let events: Vec<StreamEvent> = stream_simple(&model, &ctx, Some(simple)).collect().await;
    assert!(matches!(events.last(), Some(StreamEvent::Done { .. })));
    let _ = rx.recv().await;
    let payload = captured.lock().expect("payload slot").clone();

    // maxTokens = model cap (64000); budget = min(16384, 64000-1024).
    assert_eq!(payload["inferenceConfig"]["maxTokens"], json!(64000));
    assert_eq!(
        payload["additionalModelRequestFields"]["thinking"]["budget_tokens"],
        json!(16384)
    );

    // Adaptive Claude via stream_simple: effort path, no budget fitting.
    let opus48 = make_model(
        "global.anthropic.claude-opus-4-8-v1",
        "Claude Opus 4.8 (Global)",
        "http://unused",
        json!({}),
    );
    let captured2 = Arc::new(Mutex::new(Value::Null));
    let slot = captured2.clone();
    let simple = pir_ai::types::SimpleStreamOptions {
        stream: StreamOptions {
            env: Some(test_env()),
            on_payload: Some(Arc::new(move |payload, _model| {
                let slot = slot.clone();
                Box::pin(async move {
                    *slot.lock().expect("payload slot") = payload;
                    None
                })
            })),
            ..StreamOptions::default()
        },
        reasoning: Some(ThinkingLevel::Xhigh),
        thinking_budgets: None,
    };
    let (base_url, mut rx) = serve(
        200,
        "application/vnd.amazon.eventstream",
        minimal_stream_body(),
    )
    .await;
    let mut model = opus48;
    model.base_url = base_url;
    let events: Vec<StreamEvent> = stream_simple(&model, &ctx, Some(simple)).collect().await;
    assert!(matches!(events.last(), Some(StreamEvent::Done { .. })));
    let _ = rx.recv().await;
    let payload = captured2.lock().expect("payload slot").clone();
    assert_eq!(
        payload["additionalModelRequestFields"]["thinking"],
        json!({"type": "adaptive", "display": "summarized"})
    );
    assert_eq!(
        payload["additionalModelRequestFields"]["output_config"],
        json!({"effort": "xhigh"})
    );
}

// ---------------------------------------------------------------------------
// Error formatting
// ---------------------------------------------------------------------------

#[test]
fn test_format_bedrock_error_prefixes_and_retention_hint() {
    assert_eq!(
        format_bedrock_error(Some("ThrottlingException"), None, None, "Rate exceeded"),
        "Throttling error: Rate exceeded"
    );
    // Unmapped exception names are used as their own prefix.
    assert_eq!(
        format_bedrock_error(Some("AccessDeniedException"), None, None, "denied"),
        "AccessDeniedException: denied"
    );
    // No exception: message only.
    assert_eq!(
        format_bedrock_error(None, None, None, "connection refused"),
        "connection refused"
    );
    // Data-retention hint appended.
    let message = format_bedrock_error(
        Some("ValidationException"),
        None,
        None,
        "data retention mode 'default' is not available for this model",
    );
    assert!(message.starts_with("Validation error: "), "{message}");
    assert!(message.contains("data-retention.html"), "{message}");
}
