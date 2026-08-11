//! Contract tests for `rpi_agent::proxy` (port of
//! `external/pi/packages/agent/src/proxy.ts` @ 2efa728): drive `stream_proxy`
//! against a scripted local HTTP server and assert both sides of the contract
//! — the request shape (method / path / headers / whitelisted options body)
//! and the emitted `StreamEvent` sequence: all 12 proxy event types with
//! client-side partial reconstruction, done/error termination, non-2xx error
//! messages, abort semantics and unknown-event skipping. No real network
//! (coding-standards §12.4); the server pattern mirrors the rpi-ai
//! `contract_adapters` tests.

use std::time::Duration;

use futures::StreamExt;
use rpi_agent::proxy::{stream_proxy, ProxyStreamOptions};
use rpi_ai::types::{
    ApiKind, AssistantContent, CacheRetention, Context, DoneReason, ErrorReason, InputModality,
    Message, Model, ModelCost, StopReason, StreamEvent, TextContent, ThinkingBudgets,
    ThinkingContent, ThinkingLevel, ToolCall, Transport, Usage, UsageCost, UserContent,
    UserMessage, UserRole,
};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn test_model() -> Model {
    Model {
        id: "test-model".to_owned(),
        name: "Test Model".to_owned(),
        api: ApiKind::from("anthropic-messages"),
        provider: "test-provider".to_owned(),
        base_url: "http://test".to_owned(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![InputModality::Text],
        cost: ModelCost::default(),
        context_window: 8192,
        max_tokens: 4096,
        headers: None,
        compat: None,
        sampling_params: None,
    }
}

fn test_context() -> Context {
    Context {
        system_prompt: Some("sys".to_owned()),
        messages: vec![Message::User(UserMessage {
            role: UserRole::User,
            content: UserContent::Text("hello".to_owned()),
            timestamp: 0,
        })],
        tools: None,
    }
}

/// All eleven whitelisted fields set, so the options whitelist is fully
/// asserted.
fn test_options(proxy_url: &str) -> ProxyStreamOptions {
    ProxyStreamOptions {
        temperature: Some(0.7),
        sampling_params: Some(json!({"top_p": 0.9}).as_object().expect("object").clone()),
        max_tokens: Some(100),
        reasoning: Some(ThinkingLevel::High),
        cache_retention: Some(CacheRetention::Short),
        session_id: Some("sess-1".to_owned()),
        headers: Some(
            [("X-Custom".to_owned(), Some("v".to_owned()))]
                .into_iter()
                .collect(),
        ),
        metadata: Some(json!({"k": "v"}).as_object().expect("object").clone()),
        transport: Some(Transport::Sse),
        thinking_budgets: Some(ThinkingBudgets {
            minimal: Some(100),
            low: Some(200),
            medium: None,
            high: None,
        }),
        max_retry_delay_ms: Some(5000),
        signal: None,
        auth_token: "test-token".to_owned(),
        proxy_url: proxy_url.to_owned(),
    }
}

fn test_usage() -> Usage {
    Usage {
        input: 5,
        output: 7,
        cache_read: 1,
        cache_write: 2,
        cache_write1h: None,
        reasoning: None,
        total_tokens: 12,
        cost: UsageCost {
            input: 0.01,
            output: 0.02,
            cache_read: 0.0,
            cache_write: 0.0,
            total: 0.03,
        },
    }
}

const USAGE_JSON: &str = r#"{"input":5,"output":7,"cacheRead":1,"cacheWrite":2,"totalTokens":12,"cost":{"input":0.01,"output":0.02,"cacheRead":0,"cacheWrite":0,"total":0.03}}"#;

/// Joins proxy event JSON payloads into one SSE body, one `data: ` line each.
fn sse(events: &[&str]) -> String {
    events
        .iter()
        .map(|event| format!("data: {event}\n"))
        .collect()
}

// ---------------------------------------------------------------------------
// Scripted HTTP server (mirrors crates/rpi-ai/tests/contract_adapters.rs)
// ---------------------------------------------------------------------------

/// One captured request: request line, headers (lowercased names) and body.
#[derive(Debug)]
struct CapturedRequest {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl CapturedRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    fn body_json(&self) -> Value {
        serde_json::from_str(&self.body).expect("request body is JSON")
    }
}

/// One scripted server step after the response headers: write bytes, then
/// optionally sleep (stall used by the abort test).
enum Step {
    Write(String),
    Sleep(Duration),
}

struct Server {
    base_url: String,
    requests: mpsc::Receiver<CapturedRequest>,
}

/// Serves one HTTP response on a loopback port with scripted steps.
async fn start_server(status: u16, reason: &str, content_type: &str, steps: Vec<Step>) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (tx, rx) = mpsc::channel(1);
    let reason = reason.to_owned();
    let content_type = content_type.to_owned();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let request = read_request(&mut socket).await;
        tx.send(request).await.expect("send captured request");
        let header = format!(
            "HTTP/1.1 {status} {reason}\r\ncontent-type: {content_type}\r\nconnection: close\r\n\r\n"
        );
        socket
            .write_all(header.as_bytes())
            .await
            .expect("write header");
        for step in &steps {
            match step {
                Step::Write(bytes) => {
                    // The client may abort mid-stream and close the
                    // connection; the write then fails. The stream side owns
                    // the error path — stop serving quietly.
                    if socket.write_all(bytes.as_bytes()).await.is_err() {
                        break;
                    }
                }
                Step::Sleep(duration) => tokio::time::sleep(*duration).await,
            }
        }
    });
    Server {
        base_url: format!("http://{addr}"),
        requests: rx,
    }
}

async fn read_request(socket: &mut TcpStream) -> CapturedRequest {
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
        body: String::from_utf8_lossy(&body[..content_length]).to_string(),
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Collects a stream, failing the test if it does not end in time.
async fn collect(stream: impl futures::Stream<Item = StreamEvent> + Unpin) -> Vec<StreamEvent> {
    tokio::time::timeout(Duration::from_secs(5), stream.collect::<Vec<_>>())
        .await
        .expect("stream finished in time")
}

// ---------------------------------------------------------------------------
// Request shape and full event sequence
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_request_shape_whitelist_and_full_event_sequence() {
    let body = sse(&[
        r#"{"type":"start"}"#,
        r#"{"type":"text_start","contentIndex":0}"#,
        r#"{"type":"text_delta","contentIndex":0,"delta":"Hello "}"#,
        r#"{"type":"toolcall_start","contentIndex":1,"id":"call_1","toolName":"bash"}"#,
        r#"{"type":"text_delta","contentIndex":0,"delta":"world"}"#,
        r#"{"type":"toolcall_delta","contentIndex":1,"delta":"{\"cmd\":\"ls"}"#,
        r#"{"type":"thinking_start","contentIndex":2}"#,
        r#"{"type":"toolcall_delta","contentIndex":1,"delta":" -la\"}"}"#,
        r#"{"type":"thinking_delta","contentIndex":2,"delta":"reasoning"}"#,
        r#"{"type":"thinking_end","contentIndex":2,"contentSignature":"sig-t"}"#,
        r#"{"type":"text_end","contentIndex":0,"contentSignature":"sig-x"}"#,
        r#"{"type":"toolcall_end","contentIndex":1,"toolCall":{"type":"toolCall","id":"call_1","name":"bash","arguments":{"cmd":"ls -la"}}}"#,
        &format!(r#"{{"type":"done","reason":"stop","usage":{USAGE_JSON}}}"#),
    ]);

    let mut server = start_server(200, "OK", "text/event-stream", vec![Step::Write(body)]).await;
    let stream = stream_proxy(
        &test_model(),
        &test_context(),
        test_options(&server.base_url),
    );
    let events = collect(stream).await;

    // -- Request shape (:152-164) -----------------------------------------
    let request = server.requests.recv().await.expect("one request");
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/api/stream");
    assert_eq!(request.header("authorization"), Some("Bearer test-token"));
    assert_eq!(request.header("content-type"), Some("application/json"));

    let body = request.body_json();
    assert_eq!(body["model"]["id"], "test-model");
    assert_eq!(body["model"]["api"], "anthropic-messages");
    assert_eq!(body["model"]["provider"], "test-provider");
    assert_eq!(body["context"]["systemPrompt"], "sys");
    assert_eq!(body["context"]["messages"][0]["role"], "user");
    assert_eq!(body["context"]["messages"][0]["content"], "hello");
    // Whitelist: exactly the eleven serializable fields, nothing else
    // (`buildProxyRequestOptions`, :102-115).
    let options = body["options"].as_object().expect("options object");
    let mut keys: Vec<&String> = options.keys().collect();
    keys.sort();
    assert_eq!(
        keys,
        vec![
            "cacheRetention",
            "headers",
            "maxRetryDelayMs",
            "maxTokens",
            "metadata",
            "reasoning",
            "samplingParams",
            "sessionId",
            "temperature",
            "thinkingBudgets",
            "transport",
        ]
    );
    assert_eq!(options["temperature"], json!(0.7));
    assert_eq!(options["samplingParams"], json!({"top_p": 0.9}));
    assert_eq!(options["maxTokens"], json!(100));
    assert_eq!(options["reasoning"], json!("high"));
    assert_eq!(options["cacheRetention"], json!("short"));
    assert_eq!(options["sessionId"], json!("sess-1"));
    assert_eq!(options["headers"], json!({"X-Custom": "v"}));
    assert_eq!(options["metadata"], json!({"k": "v"}));
    assert_eq!(options["transport"], json!("sse"));
    assert_eq!(
        options["thinkingBudgets"],
        json!({"minimal": 100, "low": 200})
    );
    assert_eq!(options["maxRetryDelayMs"], json!(5000));

    // -- Event sequence and partial reconstruction (:238-353) --------------
    assert_eq!(events.len(), 13, "one event per proxy event");

    let StreamEvent::Start { partial } = &events[0] else {
        panic!("expected Start, got {:?}", events[0]);
    };
    assert_eq!(partial.stop_reason, StopReason::Pending);
    assert!(partial.content.is_empty());
    assert_eq!(partial.usage, Usage::default());
    assert_eq!(partial.api, ApiKind::from("anthropic-messages"));
    assert_eq!(partial.provider, "test-provider");
    assert_eq!(partial.model, "test-model");
    assert!(partial.timestamp > 0);

    let StreamEvent::TextStart {
        content_index,
        partial,
    } = &events[1]
    else {
        panic!("expected TextStart, got {:?}", events[1]);
    };
    assert_eq!(*content_index, 0);
    assert_eq!(
        partial.content[0],
        AssistantContent::Text(TextContent {
            text: String::new(),
            text_signature: None,
        })
    );

    let StreamEvent::TextDelta {
        content_index,
        delta,
        partial,
    } = &events[2]
    else {
        panic!("expected TextDelta, got {:?}", events[2]);
    };
    assert_eq!(*content_index, 0);
    assert_eq!(delta, "Hello ");
    assert_eq!(
        partial.content[0],
        AssistantContent::Text(TextContent {
            text: "Hello ".to_owned(),
            text_signature: None,
        })
    );

    let StreamEvent::ToolCallStart {
        content_index,
        partial,
    } = &events[3]
    else {
        panic!("expected ToolCallStart, got {:?}", events[3]);
    };
    assert_eq!(*content_index, 1);
    assert_eq!(
        partial.content[1],
        AssistantContent::ToolCall(ToolCall {
            id: "call_1".to_owned(),
            name: "bash".to_owned(),
            arguments: json!({}).as_object().expect("object").clone(),
            thought_signature: None,
            namespace: None,
        })
    );

    // Interleaved text delta accumulates across toolcall deltas
    // (correlation by content_index).
    let StreamEvent::TextDelta {
        content_index,
        delta,
        partial,
    } = &events[4]
    else {
        panic!("expected TextDelta, got {:?}", events[4]);
    };
    assert_eq!(*content_index, 0);
    assert_eq!(delta, "world");
    assert_eq!(
        partial.content[0],
        AssistantContent::Text(TextContent {
            text: "Hello world".to_owned(),
            text_signature: None,
        })
    );

    let StreamEvent::ToolCallDelta {
        content_index,
        delta,
        partial,
    } = &events[5]
    else {
        panic!("expected ToolCallDelta, got {:?}", events[5]);
    };
    assert_eq!(*content_index, 1);
    assert_eq!(delta, r#"{"cmd":"ls"#);
    assert_eq!(
        partial.content[1],
        AssistantContent::ToolCall(ToolCall {
            id: "call_1".to_owned(),
            name: "bash".to_owned(),
            arguments: json!({"cmd": "ls"}).as_object().expect("object").clone(),
            thought_signature: None,
            namespace: None,
        })
    );

    let StreamEvent::ThinkingStart {
        content_index,
        partial,
    } = &events[6]
    else {
        panic!("expected ThinkingStart, got {:?}", events[6]);
    };
    assert_eq!(*content_index, 2);
    assert_eq!(
        partial.content[2],
        AssistantContent::Thinking(ThinkingContent {
            thinking: String::new(),
            thinking_signature: None,
            redacted: None,
        })
    );

    let StreamEvent::ToolCallDelta {
        content_index,
        partial,
        ..
    } = &events[7]
    else {
        panic!("expected ToolCallDelta, got {:?}", events[7]);
    };
    assert_eq!(*content_index, 1);
    assert_eq!(
        partial.content[1],
        AssistantContent::ToolCall(ToolCall {
            id: "call_1".to_owned(),
            name: "bash".to_owned(),
            arguments: json!({"cmd": "ls -la"})
                .as_object()
                .expect("object")
                .clone(),
            thought_signature: None,
            namespace: None,
        })
    );

    let StreamEvent::ThinkingDelta {
        content_index,
        delta,
        partial,
    } = &events[8]
    else {
        panic!("expected ThinkingDelta, got {:?}", events[8]);
    };
    assert_eq!(*content_index, 2);
    assert_eq!(delta, "reasoning");
    assert_eq!(
        partial.content[2],
        AssistantContent::Thinking(ThinkingContent {
            thinking: "reasoning".to_owned(),
            thinking_signature: None,
            redacted: None,
        })
    );

    let StreamEvent::ThinkingEnd {
        content_index,
        content,
        partial,
    } = &events[9]
    else {
        panic!("expected ThinkingEnd, got {:?}", events[9]);
    };
    assert_eq!(*content_index, 2);
    assert_eq!(content, "reasoning");
    assert_eq!(
        partial.content[2],
        AssistantContent::Thinking(ThinkingContent {
            thinking: "reasoning".to_owned(),
            thinking_signature: Some("sig-t".to_owned()),
            redacted: None,
        })
    );

    let StreamEvent::TextEnd {
        content_index,
        content,
        partial,
    } = &events[10]
    else {
        panic!("expected TextEnd, got {:?}", events[10]);
    };
    assert_eq!(*content_index, 0);
    assert_eq!(content, "Hello world");
    assert_eq!(
        partial.content[0],
        AssistantContent::Text(TextContent {
            text: "Hello world".to_owned(),
            text_signature: Some("sig-x".to_owned()),
        })
    );

    let StreamEvent::ToolCallEnd {
        content_index,
        tool_call,
        partial,
    } = &events[11]
    else {
        panic!("expected ToolCallEnd, got {:?}", events[11]);
    };
    assert_eq!(*content_index, 1);
    assert_eq!(tool_call.id, "call_1");
    assert_eq!(tool_call.name, "bash");
    assert_eq!(
        tool_call.arguments,
        json!({"cmd": "ls -la"})
            .as_object()
            .expect("object")
            .clone()
    );
    // The partial keeps the finished call (without the transient partialJson).
    assert_eq!(
        partial.content[1],
        AssistantContent::ToolCall(tool_call.clone())
    );

    let StreamEvent::Done { reason, message } = &events[12] else {
        panic!("expected Done, got {:?}", events[12]);
    };
    assert_eq!(*reason, DoneReason::Stop);
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(message.usage, test_usage());
    assert_eq!(message.content.len(), 3);
    assert_eq!(
        message.content[0],
        AssistantContent::Text(TextContent {
            text: "Hello world".to_owned(),
            text_signature: Some("sig-x".to_owned()),
        })
    );
    assert_eq!(
        message.content[1],
        AssistantContent::ToolCall(ToolCall {
            id: "call_1".to_owned(),
            name: "bash".to_owned(),
            arguments: json!({"cmd": "ls -la"})
                .as_object()
                .expect("object")
                .clone(),
            thought_signature: None,
            namespace: None,
        })
    );
    assert_eq!(
        message.content[2],
        AssistantContent::Thinking(ThinkingContent {
            thinking: "reasoning".to_owned(),
            thinking_signature: Some("sig-t".to_owned()),
            redacted: None,
        })
    );
}

// ---------------------------------------------------------------------------
// Non-2xx error handling (:166-177)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_non_2xx_error_message_from_error_field() {
    let server = start_server(
        500,
        "Internal Server Error",
        "application/json",
        vec![Step::Write(r#"{"error":"boom"}"#.to_owned())],
    )
    .await;
    let stream = stream_proxy(
        &test_model(),
        &test_context(),
        test_options(&server.base_url),
    );
    let events = collect(stream).await;

    assert_eq!(events.len(), 1);
    let StreamEvent::Error { reason, error } = &events[0] else {
        panic!("expected Error, got {:?}", events[0]);
    };
    assert_eq!(*reason, ErrorReason::Error);
    assert_eq!(error.stop_reason, StopReason::Error);
    assert_eq!(error.error_message.as_deref(), Some("Proxy error: boom"));
    // The partial carried into the error event keeps the initial shape.
    assert_eq!(error.usage, Usage::default());
    assert!(error.content.is_empty());
}

#[tokio::test]
async fn test_non_2xx_error_message_falls_back_to_status_line() {
    // Non-JSON body: the `{ error }` probe fails and the status text remains.
    let server = start_server(
        400,
        "Bad Request",
        "text/plain",
        vec![Step::Write("nope".to_owned())],
    )
    .await;
    let stream = stream_proxy(
        &test_model(),
        &test_context(),
        test_options(&server.base_url),
    );
    let events = collect(stream).await;

    assert_eq!(events.len(), 1);
    let StreamEvent::Error { reason, error } = &events[0] else {
        panic!("expected Error, got {:?}", events[0]);
    };
    assert_eq!(*reason, ErrorReason::Error);
    assert_eq!(
        error.error_message.as_deref(),
        Some("Proxy error: 400 Bad Request")
    );
}

// ---------------------------------------------------------------------------
// Malformed data and unknown event types (:195-206, :361-365)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_malformed_data_line_ends_stream_with_error() {
    let server = start_server(
        200,
        "OK",
        "text/event-stream",
        vec![
            Step::Write(sse(&[
                r#"{"type":"start"}"#,
                r#"{"type":"text_start","contentIndex":0}"#,
            ])),
            // `JSON.parse` fails -> the whole stream errors (:199/:214).
            Step::Write("data: not-json\n".to_owned()),
        ],
    )
    .await;
    let stream = stream_proxy(
        &test_model(),
        &test_context(),
        test_options(&server.base_url),
    );
    let events = collect(stream).await;

    assert_eq!(events.len(), 3);
    assert!(matches!(events[0], StreamEvent::Start { .. }));
    assert!(matches!(events[1], StreamEvent::TextStart { .. }));
    let StreamEvent::Error { reason, error } = &events[2] else {
        panic!("expected Error, got {:?}", events[2]);
    };
    assert_eq!(*reason, ErrorReason::Error);
    assert_eq!(error.stop_reason, StopReason::Error);
    let message = error.error_message.as_deref().expect("error message");
    assert!(message.contains("expected"), "message was: {message}");
    // The accumulated partial is preserved in the error event.
    assert_eq!(
        error.content[0],
        AssistantContent::Text(TextContent {
            text: String::new(),
            text_signature: None,
        })
    );
}

#[tokio::test]
async fn test_unknown_event_types_are_warned_and_skipped() {
    let server = start_server(
        200,
        "OK",
        "text/event-stream",
        vec![Step::Write(sse(&[
            r#"{"type":"start"}"#,
            r#"{"type":"bogus","contentIndex":9}"#,
            r#"{"type":"text_start","contentIndex":0}"#,
            r#"{"type":"somethingElse","x":1}"#,
            &format!(r#"{{"type":"done","reason":"toolUse","usage":{USAGE_JSON}}}"#),
        ]))],
    )
    .await;
    let stream = stream_proxy(
        &test_model(),
        &test_context(),
        test_options(&server.base_url),
    );
    let events = collect(stream).await;

    assert_eq!(events.len(), 3, "unknown events produce no stream events");
    assert!(matches!(events[0], StreamEvent::Start { .. }));
    assert!(matches!(events[1], StreamEvent::TextStart { .. }));
    let StreamEvent::Done { reason, message } = &events[2] else {
        panic!("expected Done, got {:?}", events[2]);
    };
    assert_eq!(*reason, DoneReason::ToolUse);
    assert_eq!(message.stop_reason, StopReason::ToolUse);
}

#[tokio::test]
async fn test_trailing_line_without_newline_is_dropped() {
    // Upstream keeps the last incomplete line in `buffer` and never processes
    // it (:193/:207): the `done` event below must not be emitted.
    let done_event = format!(r#"{{"type":"done","reason":"stop","usage":{USAGE_JSON}}}"#);
    let body = format!("data: {}\ndata: {}", r#"{"type":"start"}"#, done_event);
    let server = start_server(200, "OK", "text/event-stream", vec![Step::Write(body)]).await;
    let stream = stream_proxy(
        &test_model(),
        &test_context(),
        test_options(&server.base_url),
    );
    let events = collect(stream).await;

    assert_eq!(events.len(), 1);
    assert!(matches!(events[0], StreamEvent::Start { .. }));
}

#[tokio::test]
async fn test_stream_ends_cleanly_without_terminal_event() {
    // A server may close the stream without a `done`/`error` proxy event;
    // `stream.end()` still closes the event stream (:213).
    let server = start_server(
        200,
        "OK",
        "text/event-stream",
        vec![Step::Write(sse(&[r#"{"type":"start"}"#]))],
    )
    .await;
    let stream = stream_proxy(
        &test_model(),
        &test_context(),
        test_options(&server.base_url),
    );
    let events = collect(stream).await;

    assert_eq!(events.len(), 1);
    assert!(matches!(events[0], StreamEvent::Start { .. }));
}

// ---------------------------------------------------------------------------
// Terminal events from the server (:350-359)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_server_error_event_terminates_and_drops_following_events() {
    let server = start_server(
        200,
        "OK",
        "text/event-stream",
        vec![Step::Write(sse(&[
            r#"{"type":"start"}"#,
            r#"{"type":"text_start","contentIndex":0}"#,
            &format!(
                r#"{{"type":"error","reason":"error","errorMessage":"server oops","usage":{USAGE_JSON}}}"#
            ),
            // Anything after the terminal event is dropped by the stream
            // (upstream EventStream done flag).
            r#"{"type":"text_delta","contentIndex":0,"delta":"ignored"}"#,
        ]))],
    )
    .await;
    let stream = stream_proxy(
        &test_model(),
        &test_context(),
        test_options(&server.base_url),
    );
    let events = collect(stream).await;

    assert_eq!(
        events.len(),
        3,
        "events after the terminal error are dropped"
    );
    assert!(matches!(events[0], StreamEvent::Start { .. }));
    assert!(matches!(events[1], StreamEvent::TextStart { .. }));
    let StreamEvent::Error { reason, error } = &events[2] else {
        panic!("expected Error, got {:?}", events[2]);
    };
    assert_eq!(*reason, ErrorReason::Error);
    assert_eq!(error.stop_reason, StopReason::Error);
    assert_eq!(error.error_message.as_deref(), Some("server oops"));
    assert_eq!(error.usage, test_usage());
    assert_eq!(
        error.content[0],
        AssistantContent::Text(TextContent {
            text: String::new(),
            text_signature: None,
        })
    );
}

// ---------------------------------------------------------------------------
// Abort semantics (:141-149, :209-218)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_abort_mid_stream_ends_with_aborted_error() {
    let server = start_server(
        200,
        "OK",
        "text/event-stream",
        vec![
            Step::Write(sse(&[
                r#"{"type":"start"}"#,
                r#"{"type":"text_start","contentIndex":0}"#,
            ])),
            // Stall: the client must cancel the pending body read.
            Step::Sleep(Duration::from_secs(60)),
        ],
    )
    .await;
    let token = CancellationToken::new();
    let mut options = test_options(&server.base_url);
    options.signal = Some(token.clone());
    let mut stream = stream_proxy(&test_model(), &test_context(), options);

    let first = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("start arrives")
        .expect("stream not ended yet");
    assert!(matches!(first, StreamEvent::Start { .. }));
    let second = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("text_start arrives")
        .expect("stream not ended yet");
    assert!(matches!(second, StreamEvent::TextStart { .. }));

    token.cancel();
    let rest = tokio::time::timeout(Duration::from_secs(5), stream.collect::<Vec<_>>())
        .await
        .expect("abort lands");

    assert_eq!(rest.len(), 1);
    let StreamEvent::Error { reason, error } = &rest[0] else {
        panic!("expected Error, got {:?}", rest[0]);
    };
    assert_eq!(*reason, ErrorReason::Aborted);
    assert_eq!(error.stop_reason, StopReason::Aborted);
    assert_eq!(
        error.error_message.as_deref(),
        Some("Request aborted by user")
    );
    // The partial accumulated before the abort is preserved.
    assert_eq!(
        error.content[0],
        AssistantContent::Text(TextContent {
            text: String::new(),
            text_signature: None,
        })
    );
}

#[tokio::test]
async fn test_abort_before_request_ends_with_aborted_error() {
    // A pre-cancelled signal aborts the request itself (:163); the URL is
    // never contacted.
    let token = CancellationToken::new();
    token.cancel();
    let mut options = test_options("http://127.0.0.1:1");
    options.signal = Some(token.clone());
    let stream = stream_proxy(&test_model(), &test_context(), options);
    let events = collect(stream).await;

    assert_eq!(events.len(), 1);
    let StreamEvent::Error { reason, error } = &events[0] else {
        panic!("expected Error, got {:?}", events[0]);
    };
    assert_eq!(*reason, ErrorReason::Aborted);
    assert_eq!(error.stop_reason, StopReason::Aborted);
    assert_eq!(
        error.error_message.as_deref(),
        Some("Request aborted by user")
    );
}

// ---------------------------------------------------------------------------
// Protocol violations (:250-348)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_delta_for_wrong_content_type_errors() {
    let cases: Vec<(Vec<&str>, &str)> = vec![
        (
            vec![
                r#"{"type":"toolcall_start","contentIndex":0,"id":"c1","toolName":"bash"}"#,
                r#"{"type":"text_delta","contentIndex":0,"delta":"x"}"#,
            ],
            "Received text_delta for non-text content",
        ),
        (
            vec![
                r#"{"type":"text_start","contentIndex":0}"#,
                r#"{"type":"thinking_delta","contentIndex":0,"delta":"x"}"#,
            ],
            "Received thinking_delta for non-thinking content",
        ),
        (
            vec![
                r#"{"type":"text_start","contentIndex":0}"#,
                r#"{"type":"toolcall_delta","contentIndex":0,"delta":"{}"}"#,
            ],
            "Received toolcall_delta for non-toolCall content",
        ),
        (
            vec![
                r#"{"type":"toolcall_start","contentIndex":0,"id":"c1","toolName":"bash"}"#,
                r#"{"type":"text_end","contentIndex":0}"#,
            ],
            "Received text_end for non-text content",
        ),
    ];
    for (script, expected_message) in cases {
        let server = start_server(
            200,
            "OK",
            "text/event-stream",
            vec![Step::Write(sse(&[
                r#"{"type":"start"}"#,
                script[0],
                script[1],
            ]))],
        )
        .await;
        let stream = stream_proxy(
            &test_model(),
            &test_context(),
            test_options(&server.base_url),
        );
        let events = collect(stream).await;
        let last = events.last().expect("terminal event");
        let StreamEvent::Error { reason, error } = last else {
            panic!("expected Error, got {last:?}");
        };
        assert_eq!(*reason, ErrorReason::Error);
        assert_eq!(error.error_message.as_deref(), Some(expected_message));
    }
}

#[tokio::test]
async fn test_toolcall_end_on_non_toolcall_content_is_skipped() {
    // Unlike deltas, `toolcall_end` for non-toolCall content is silently
    // skipped (:347), not an error.
    let server = start_server(
        200,
        "OK",
        "text/event-stream",
        vec![Step::Write(sse(&[
            r#"{"type":"start"}"#,
            r#"{"type":"text_start","contentIndex":0}"#,
            r#"{"type":"toolcall_end","contentIndex":0,"toolCall":{"type":"toolCall","id":"c1","name":"bash","arguments":{}}}"#,
            &format!(r#"{{"type":"done","reason":"length","usage":{USAGE_JSON}}}"#),
        ]))],
    )
    .await;
    let stream = stream_proxy(
        &test_model(),
        &test_context(),
        test_options(&server.base_url),
    );
    let events = collect(stream).await;

    assert_eq!(events.len(), 3, "toolcall_end on text is skipped");
    assert!(matches!(events[0], StreamEvent::Start { .. }));
    assert!(matches!(events[1], StreamEvent::TextStart { .. }));
    let StreamEvent::Done { reason, message } = &events[2] else {
        panic!("expected Done, got {:?}", events[2]);
    };
    assert_eq!(*reason, DoneReason::Length);
    assert_eq!(message.stop_reason, StopReason::Length);
    // The text block was untouched by the stray toolcall_end.
    assert_eq!(
        message.content[0],
        AssistantContent::Text(TextContent {
            text: String::new(),
            text_signature: None,
        })
    );
}

// ---------------------------------------------------------------------------
// toolcall_end merge semantics (proxy.test.ts:34-82 @ 4181f66)
// ---------------------------------------------------------------------------

/// Port of upstream `streamProxy > preserves tool-call metadata received only
/// on toolcall_end` (packages/agent/test/proxy.test.ts:34-82 @ 4181f66): the
/// server-authoritative `toolCall` on the `toolcall_end` frame replaces the
/// accumulated block, so metadata that only exists server-side (here
/// `namespace`) survives into both the event and the final message.
#[tokio::test]
async fn preserves_tool_call_metadata_received_only_on_toolcall_end() {
    let body = sse(&[
        r#"{"type":"start"}"#,
        r#"{"type":"toolcall_start","contentIndex":0,"id":"call_test|fc_test","toolName":"lookup"}"#,
        r#"{"type":"toolcall_delta","contentIndex":0,"delta":"{\"value\":\"hello\"}"}"#,
        r#"{"type":"toolcall_end","contentIndex":0,"toolCall":{"type":"toolCall","id":"call_test|fc_test","name":"lookup","arguments":{"value":"hello"},"namespace":"dynamic_tools"}}"#,
        &format!(r#"{{"type":"done","reason":"toolUse","usage":{USAGE_JSON}}}"#),
    ]);
    let server = start_server(200, "OK", "text/event-stream", vec![Step::Write(body)]).await;
    let stream = stream_proxy(
        &test_model(),
        &test_context(),
        test_options(&server.base_url),
    );
    let events = collect(stream).await;

    let end_event = events
        .iter()
        .find(|event| matches!(event, StreamEvent::ToolCallEnd { .. }))
        .expect("toolcall_end event");
    let StreamEvent::ToolCallEnd { tool_call, .. } = end_event else {
        unreachable!();
    };
    assert_eq!(tool_call.namespace.as_deref(), Some("dynamic_tools"));

    let StreamEvent::Done { message, .. } = events.last().expect("done event") else {
        panic!("expected Done, got {:?}", events.last());
    };
    assert_eq!(
        message.content[0],
        AssistantContent::ToolCall(ToolCall {
            id: "call_test|fc_test".to_owned(),
            name: "lookup".to_owned(),
            arguments: json!({"value": "hello"})
                .as_object()
                .expect("object")
                .clone(),
            thought_signature: None,
            namespace: Some("dynamic_tools".to_owned()),
        })
    );
}
