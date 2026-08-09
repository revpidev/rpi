//! Contract tests for the mistral-conversations adapter: drive `stream()` /
//! `stream_simple()` over a scripted local HTTP server with recorded SSE
//! streams, and assert both sides of the contract — the request shape
//! (method / path / key headers / body JSON) and the emitted `StreamEvent`
//! sequence. Mirrors `contract_adapters.rs`; the SSE payloads are recorded in
//! the upstream Mistral wire format (`@mistralai/mistralai` chunk schemas).

use std::time::Duration;

use futures::StreamExt;
use rpi_ai::api::mistral_conversations::MistralConversations;
use rpi_ai::models::ProviderStreams;
use rpi_ai::types::{
    ApiKind, CacheRetention, Context, Message, Model, SimpleStreamOptions, StopReason, StreamEvent,
    StreamOptions, ThinkingLevel,
};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

// ---------------------------------------------------------------------------
// Scripted HTTP server
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

/// Serves the scripted `(status, body)` responses, one per connection, on a
/// loopback port. Returns the base URL and a channel of captured requests.
async fn serve(script: Vec<(u16, &'static str)>) -> (String, mpsc::Receiver<CapturedRequest>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (tx, rx) = mpsc::channel(script.len().max(1));
    tokio::spawn(async move {
        for (status, body) in script {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let request = read_request(&mut socket).await;
            tx.send(request).await.expect("send captured request");
            let reason = match status {
                200 => "OK",
                400 => "Bad Request",
                429 => "Too Many Requests",
                _ => "Status",
            };
            let content_type = if status == 200 {
                "text/event-stream"
            } else {
                "application/json"
            };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write response");
        }
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
        body: String::from_utf8_lossy(&body[..content_length]).to_string(),
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

fn model(id: &str, base_url: &str, extra: Value) -> Model {
    let mut value = json!({
        "id": id, "name": id, "api": ApiKind::MISTRAL_CONVERSATIONS, "provider": "mistral",
        "baseUrl": base_url, "reasoning": false, "input": ["text"],
        "cost": {"input": 2.0, "output": 6.0, "cacheRead": 0.2, "cacheWrite": 2.0},
        "contextWindow": 128000, "maxTokens": 8192
    });
    value
        .as_object_mut()
        .expect("object")
        .extend(extra.as_object().cloned().unwrap_or_default());
    serde_json::from_value(value).expect("model")
}

fn context(messages: Vec<Message>) -> Context {
    Context {
        system_prompt: None,
        messages,
        tools: None,
    }
}

fn user_text(text: &str) -> Message {
    serde_json::from_value(json!({"role": "user", "content": text, "timestamp": 0})).expect("user")
}

fn options() -> StreamOptions {
    StreamOptions {
        api_key: Some("test-key".to_owned()),
        ..StreamOptions::default()
    }
}

fn simple_options(reasoning: Option<ThinkingLevel>, stream: StreamOptions) -> SimpleStreamOptions {
    SimpleStreamOptions {
        stream,
        reasoning,
        thinking_budgets: None,
    }
}

fn event_kinds(events: &[StreamEvent]) -> Vec<&str> {
    events
        .iter()
        .map(|event| match event {
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
        })
        .collect()
}

async fn collect(
    stream: rpi_ai::utils::event_stream::AssistantMessageEventStream,
) -> Vec<StreamEvent> {
    tokio::time::timeout(Duration::from_secs(10), stream.collect())
        .await
        .expect("stream completes within 10s")
}

// ---------------------------------------------------------------------------
// Recorded SSE streams
// ---------------------------------------------------------------------------

const TEXT_SSE: &str = concat!(
    "data: {\"id\":\"cmpl-1\",\"model\":\"mistral-large-latest\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hello\"},\"finish_reason\":null}]}\n",
    "\n",
    "data: {\"id\":\"cmpl-1\",\"model\":\"mistral-large-latest\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" world\"},\"finish_reason\":null}]}\n",
    "\n",
    "data: {\"id\":\"cmpl-1\",\"model\":\"mistral-large-latest\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15,\"prompt_tokens_details\":{\"cached_tokens\":4}}}\n",
    "\n",
    "data: [DONE]\n",
    "\n",
);

const THINKING_SSE: &str = concat!(
    "data: {\"id\":\"cmpl-2\",\"model\":\"magistral-medium-latest\",\"choices\":[{\"index\":0,\"delta\":{\"content\":[{\"type\":\"thinking\",\"thinking\":[{\"type\":\"text\",\"text\":\"hmm\"}]}]},\"finish_reason\":null}]}\n",
    "\n",
    "data: {\"id\":\"cmpl-2\",\"model\":\"magistral-medium-latest\",\"choices\":[{\"index\":0,\"delta\":{\"content\":[{\"type\":\"text\",\"text\":\"answer\"}]},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":8,\"completion_tokens\":3,\"total_tokens\":11}}\n",
    "\n",
    "data: [DONE]\n",
    "\n",
);

const TOOL_CALL_SSE: &str = concat!(
    "data: {\"id\":\"cmpl-3\",\"model\":\"mistral-large-latest\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"abc123XYZ\",\"type\":\"function\",\"function\":{\"name\":\"bash\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n",
    "\n",
    "data: {\"id\":\"cmpl-3\",\"model\":\"mistral-large-latest\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"abc123XYZ\",\"function\":{\"name\":\"bash\",\"arguments\":\"{\\\"cmd\\\":\"}}]},\"finish_reason\":null}]}\n",
    "\n",
    "data: {\"id\":\"cmpl-3\",\"model\":\"mistral-large-latest\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"abc123XYZ\",\"function\":{\"name\":\"bash\",\"arguments\":\"\\\"ls\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}\n",
    "\n",
    "data: [DONE]\n",
    "\n",
);

// ---------------------------------------------------------------------------
// Contract tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_mistral_conversations_contract() {
    let (base_url, mut captured) = serve(vec![(200, TEXT_SSE)]).await;
    let m = model("mistral-large-latest", &base_url, json!({}));
    let mut opts = options();
    opts.session_id = Some("session-123".to_owned());
    let events =
        collect(MistralConversations.stream(&m, &context(vec![user_text("hi")]), Some(opts))).await;

    let request = captured.recv().await.expect("request captured");
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/v1/chat/completions");
    assert_eq!(request.header("authorization"), Some("Bearer test-key"));
    assert_eq!(request.header("accept"), Some("text/event-stream"));
    // Session-based prompt caching: header + body key (mistral-conversations.ts:231-235,262).
    assert_eq!(request.header("x-affinity"), Some("session-123"));
    let body = request.body_json();
    assert_eq!(body["model"], json!("mistral-large-latest"));
    assert_eq!(body["stream"], json!(true));
    assert_eq!(body["prompt_cache_key"], json!("session-123"));
    assert_eq!(body["messages"][0]["role"], json!("user"));

    assert_eq!(
        event_kinds(&events),
        vec![
            "start",
            "text_start",
            "text_delta",
            "text_delta",
            "text_end",
            "done"
        ]
    );
    let StreamEvent::Done { reason, message } = &events[5] else {
        panic!("expected done event");
    };
    assert_eq!(*reason, rpi_ai::types::DoneReason::Stop);
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(message.response_id.as_deref(), Some("cmpl-1"));
    // Cached tokens split out of input (mistral-conversations.ts:337-348).
    assert_eq!(message.usage.input, 6);
    assert_eq!(message.usage.cache_read, 4);
    assert_eq!(message.usage.cache_write, 0);
    assert_eq!(message.usage.output, 5);
    assert_eq!(message.usage.total_tokens, 15);
}

#[tokio::test]
async fn test_mistral_prompt_mode_reasoning_stream() {
    // mistral-reasoning-mode.test.ts intent: Magistral models stream with
    // `prompt_mode: "reasoning"` and produce thinking blocks.
    let (base_url, mut captured) = serve(vec![(200, THINKING_SSE)]).await;
    let m = model(
        "magistral-medium-latest",
        &base_url,
        json!({"reasoning": true}),
    );
    let events = collect(MistralConversations.stream_simple(
        &m,
        &context(vec![user_text("hi")]),
        Some(simple_options(Some(ThinkingLevel::Medium), options())),
    ))
    .await;

    let request = captured.recv().await.expect("request captured");
    let body = request.body_json();
    assert_eq!(body["prompt_mode"], json!("reasoning"));
    assert!(body.get("reasoning_effort").is_none());
    // No session id: no prompt caching.
    assert!(body.get("prompt_cache_key").is_none());
    assert!(request.header("x-affinity").is_none());

    assert_eq!(
        event_kinds(&events),
        vec![
            "start",
            "thinking_start",
            "thinking_delta",
            "thinking_end",
            "text_start",
            "text_delta",
            "text_end",
            "done"
        ]
    );
}

#[tokio::test]
async fn test_mistral_reasoning_effort_stream() {
    // mistral-reasoning-mode.test.ts intent: Mistral Small 4 / Medium 3.5 use
    // `reasoning_effort` instead of `prompt_mode`.
    let (base_url, mut captured) = serve(vec![(200, TEXT_SSE)]).await;
    let m = model("mistral-small-2603", &base_url, json!({"reasoning": true}));
    let events = collect(MistralConversations.stream_simple(
        &m,
        &context(vec![user_text("hi")]),
        Some(simple_options(Some(ThinkingLevel::Medium), options())),
    ))
    .await;

    let request = captured.recv().await.expect("request captured");
    let body = request.body_json();
    assert_eq!(body["reasoning_effort"], json!("high"));
    assert!(body.get("prompt_mode").is_none());
    assert_eq!(event_kinds(&events).last(), Some(&"done"));
}

#[tokio::test]
async fn test_mistral_tool_call_stream_and_id_normalization() {
    let (base_url, mut captured) = serve(vec![(200, TOOL_CALL_SSE)]).await;
    let m = model("mistral-large-latest", &base_url, json!({}));

    // Cross-provider history: the OpenAI tool-call id is normalized to the
    // 9-char alphanumeric Mistral shape, and the tool result id back-filled
    // (mistral-conversations.ts:157-187 via transformMessages).
    let history: Vec<Message> = serde_json::from_value(json!([
        {
            "role": "assistant", "api": "openai-completions", "provider": "openai",
            "model": "gpt-4o", "timestamp": 0, "stopReason": "toolUse",
            "usage": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 0,
                      "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}},
            "content": [{"type": "toolCall", "id": "call_long_openai_id_with_underscores", "name": "bash", "arguments": {"cmd": "ls"}}]
        },
        {
            "role": "toolResult", "toolCallId": "call_long_openai_id_with_underscores",
            "toolName": "bash", "content": [{"type": "text", "text": "ok"}],
            "isError": false, "timestamp": 0
        },
        {"role": "user", "content": "again", "timestamp": 0}
    ]))
    .expect("history");

    let events = collect(MistralConversations.stream(&m, &context(history), Some(options()))).await;

    let request = captured.recv().await.expect("request captured");
    let body = request.body_json();
    let normalized_id = body["messages"][0]["tool_calls"][0]["id"]
        .as_str()
        .expect("tool call id");
    assert_eq!(normalized_id.len(), 9);
    assert!(normalized_id.chars().all(|c| c.is_ascii_alphanumeric()));
    // The tool result carries the same normalized id.
    assert_eq!(body["messages"][1]["tool_call_id"], json!(normalized_id));
    assert_eq!(body["messages"][1]["role"], json!("tool"));

    assert_eq!(
        event_kinds(&events),
        vec![
            "start",
            "toolcall_start",
            "toolcall_delta",
            "toolcall_delta",
            "toolcall_delta",
            "toolcall_end",
            "done"
        ]
    );
    let StreamEvent::Done { reason, message } = events.last().expect("done") else {
        panic!("expected done event");
    };
    assert_eq!(*reason, rpi_ai::types::DoneReason::ToolUse);
    assert_eq!(message.stop_reason, StopReason::ToolUse);
    let rpi_ai::types::AssistantContent::ToolCall(call) = &message.content[0] else {
        panic!("expected tool call block");
    };
    assert_eq!(call.id, "abc123XYZ");
    assert_eq!(call.name, "bash");
    assert_eq!(
        serde_json::Value::Object(call.arguments.clone()),
        json!({"cmd": "ls"})
    );
}

#[tokio::test]
async fn test_mistral_error_stream() {
    let error_body = "{\"message\":\"Invalid model\",\"type\":\"invalid_request_error\"}";
    let (base_url, mut captured) = serve(vec![(400, error_body)]).await;
    let m = model("mistral-large-latest", &base_url, json!({}));
    let events =
        collect(MistralConversations.stream(&m, &context(vec![user_text("hi")]), Some(options())))
            .await;

    captured.recv().await.expect("request captured");
    assert_eq!(event_kinds(&events), vec!["error"]);
    let StreamEvent::Error { reason, error } = &events[0] else {
        panic!("expected error event");
    };
    assert_eq!(*reason, rpi_ai::types::ErrorReason::Error);
    assert_eq!(error.stop_reason, StopReason::Error);
    // formatMistralError: status + trimmed body (mistral-conversations.ts:189-201).
    assert_eq!(
        error.error_message,
        Some(format!("Mistral API error (400): {error_body}"))
    );
}

#[tokio::test]
async fn test_mistral_cache_retention_none_omits_prompt_caching() {
    // mistral-reasoning-mode.test.ts intent: cacheRetention "none" omits both
    // the promptCacheKey body field and the x-affinity header.
    let (base_url, mut captured) = serve(vec![(200, TEXT_SSE)]).await;
    let m = model("mistral-large-latest", &base_url, json!({}));
    let mut opts = options();
    opts.session_id = Some("session-123".to_owned());
    opts.cache_retention = Some(CacheRetention::None);
    let events =
        collect(MistralConversations.stream(&m, &context(vec![user_text("hi")]), Some(opts))).await;

    let request = captured.recv().await.expect("request captured");
    assert!(request.header("x-affinity").is_none());
    let body = request.body_json();
    assert!(body.get("prompt_cache_key").is_none());
    assert_eq!(event_kinds(&events).last(), Some(&"done"));
}

#[tokio::test]
async fn test_mistral_tool_schema_strict_serialization() {
    // mistral-tool-schema.test.ts intent: a `json_schema`/`require`
    // constrained-sampling tool serializes with `"strict": true` and plain
    // object parameters (no TypeBox symbol keys — impossible in Rust).
    let (base_url, mut captured) = serve(vec![(200, TEXT_SSE)]).await;
    let m = model("devstral-medium-latest", &base_url, json!({}));
    let tool: rpi_ai::types::Tool = serde_json::from_value(json!({
        "name": "inspect_schema",
        "description": "Inspect the schema",
        "parameters": {"type": "object", "properties": {"nested": {"type": "object", "properties": {"value": {"type": "string"}}}}},
        "constrainedSampling": {"type": "json_schema", "strict": "require"}
    }))
    .expect("tool");
    let ctx = Context {
        system_prompt: None,
        messages: vec![user_text("Hi")],
        tools: Some(vec![tool]),
    };
    let events = collect(MistralConversations.stream(&m, &ctx, Some(options()))).await;

    let request = captured.recv().await.expect("request captured");
    let body = request.body_json();
    let tools = body["tools"].as_array().expect("tools");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["type"], json!("function"));
    assert_eq!(tools[0]["function"]["strict"], json!(true));
    assert_eq!(
        tools[0]["function"]["parameters"]["properties"]["nested"]["properties"]["value"]["type"],
        json!("string")
    );
    assert_eq!(event_kinds(&events).last(), Some(&"done"));
}
