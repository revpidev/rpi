//! Contract tests for the Google Generative AI adapter (T13 W1), mirroring
//! the upstream suites `google-shared-convert-tools.test.ts`,
//! `google-shared-gemini3-unsigned-tool-call.test.ts`,
//! `google-shared-image-tool-result-routing.test.ts`,
//! `google-thinking-disable.test.ts` (E2E intent ported to wire-shape
//! assertions — upstream's live tests are env-gated) and
//! `google-thinking-signature.test.ts`, plus the requirements §5.2 anchors:
//! the thinking budget table, the usage mapping, thoughtSignature retention,
//! non-streaming function calls, and self-incrementing tool call ids.
//!
//! The SSE payloads use the Gemini API wire format (the shapes the pinned
//! `@google/genai` SDK feeds through `generateContentResponseFromMldev`).

use std::time::Duration;

use futures::StreamExt;
use pir_ai::api::google_generative_ai::{stream_simple, GoogleGenerativeAi};
use pir_ai::api::google_shared::{
    convert_messages, convert_tools, is_thinking_part, map_stop_reason, map_stop_reason_string,
    requires_tool_call_id, resolve_google_function_calling_mode, retain_thought_signature,
    supports_google_strict_tool_sampling,
};
use pir_ai::models::ProviderStreams;
use pir_ai::types::{
    ApiKind, Context, Message, Model, SimpleStreamOptions, StopReason, StreamEvent, StreamOptions,
    ThinkingBudgets, ThinkingLevel, Tool,
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

fn model_with_id(id: &str, base_url: &str, extra: Value) -> Model {
    let mut value = json!({
        "id": id, "name": id, "api": ApiKind::GOOGLE_GENERATIVE_AI, "provider": "google",
        "baseUrl": base_url, "reasoning": true, "input": ["text"],
        "cost": {"input": 1.0, "output": 2.0, "cacheRead": 0.5, "cacheWrite": 1.0},
        "contextWindow": 128000, "maxTokens": 8192
    });
    value
        .as_object_mut()
        .expect("object")
        .extend(extra.as_object().cloned().unwrap_or_default());
    serde_json::from_value(value).expect("model")
}

fn model(base_url: &str) -> Model {
    model_with_id("test-model", base_url, json!({}))
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

fn simple_options(
    reasoning: Option<ThinkingLevel>,
    budgets: Option<ThinkingBudgets>,
) -> SimpleStreamOptions {
    SimpleStreamOptions {
        stream: options(),
        reasoning,
        thinking_budgets: budgets,
    }
}

fn make_tool(parameters: Value) -> Tool {
    serde_json::from_value(json!({
        "name": "test_tool", "description": "A test tool", "parameters": parameters
    }))
    .expect("tool")
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
    stream: pir_ai::utils::event_stream::AssistantMessageEventStream,
) -> Vec<StreamEvent> {
    tokio::time::timeout(Duration::from_secs(10), stream.collect())
        .await
        .expect("stream completes within 10s")
}

// ---------------------------------------------------------------------------
// Recorded SSE streams (Gemini API wire format)
// ---------------------------------------------------------------------------

const GOOGLE_TEXT_SSE: &str = concat!(
    "data: {\"responseId\":\"resp-1\",\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"Hello\"}]}}]}\n",
    "\n",
    "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\" world\"}]}}],\"usageMetadata\":{\"promptTokenCount\":10,\"cachedContentTokenCount\":4,\"candidatesTokenCount\":5,\"thoughtsTokenCount\":2,\"totalTokenCount\":21}}\n",
    "\n",
    "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[]},\"finishReason\":\"STOP\"}]}\n",
    "\n",
);

const GOOGLE_THINKING_SSE: &str = concat!(
    "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"thought\":true,\"text\":\"thinking...\",\"thoughtSignature\":\"QUJDREVGRw==\"}]}}]}\n",
    "\n",
    "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"thought\":true,\"text\":\" more\"}]}}]}\n",
    "\n",
    "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"Answer\"}]}}]}\n",
    "\n",
    "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":8,\"candidatesTokenCount\":3,\"thoughtsTokenCount\":12,\"totalTokenCount\":23}}\n",
    "\n",
);

const GOOGLE_TOOL_CALL_SSE: &str = concat!(
    "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"Let me check.\"}]}}]}\n",
    "\n",
    "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"functionCall\":{\"name\":\"bash\",\"args\":{\"command\":\"ls\"}},\"thoughtSignature\":\"QUJDREVGRw==\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":10,\"candidatesTokenCount\":5,\"totalTokenCount\":15}}\n",
    "\n",
);

/// Ends without any finish reason.
const GOOGLE_NO_FINISH_SSE: &str = concat!(
    "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"dangling\"}]}}]}\n",
    "\n",
);

// ---------------------------------------------------------------------------
// 正常流 contract
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_google_generative_ai_contract() {
    let (base_url, mut captured) = serve(vec![(200, GOOGLE_TEXT_SSE)]).await;
    let m = model(&base_url);
    let events =
        collect(GoogleGenerativeAi.stream(&m, &context(vec![user_text("hi")]), Some(options())))
            .await;

    let request = captured.recv().await.expect("request captured");
    assert_eq!(request.method, "POST");
    // SDK-derived path: {baseUrl}/models/{model}:streamGenerateContent?alt=sse.
    assert_eq!(
        request.path,
        "/models/test-model:streamGenerateContent?alt=sse"
    );
    assert_eq!(request.header("x-goog-api-key"), Some("test-key"));
    let body = request.body_json();
    // The model id goes to the URL, not the body.
    assert!(body.get("model").is_none());
    assert_eq!(body["contents"][0]["role"], json!("user"));
    assert_eq!(body["contents"][0]["parts"][0]["text"], json!("hi"));
    // No generationConfig without temperature/maxTokens/thinking.
    assert!(body.get("generationConfig").is_none());

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
    assert_eq!(*reason, pir_ai::types::DoneReason::Stop);
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(message.response_id.as_deref(), Some("resp-1"));
    // Usage mapping anchor (requirements §5.2):
    // input = promptTokenCount − cachedContentTokenCount,
    // output = candidatesTokenCount + thoughtsTokenCount,
    // cacheRead = cachedContentTokenCount, cacheWrite = 0,
    // reasoning = thoughtsTokenCount, totalTokens = totalTokenCount.
    assert_eq!(message.usage.input, 6);
    assert_eq!(message.usage.output, 7);
    assert_eq!(message.usage.cache_read, 4);
    assert_eq!(message.usage.cache_write, 0);
    assert_eq!(message.usage.reasoning, Some(2));
    assert_eq!(message.usage.total_tokens, 21);
}

#[tokio::test]
async fn test_google_generative_ai_system_prompt_and_generation_config() {
    let (base_url, mut captured) = serve(vec![(200, GOOGLE_TEXT_SSE)]).await;
    let m = model(&base_url);
    let mut ctx = context(vec![user_text("hi")]);
    ctx.system_prompt = Some("Be brief.".to_owned());
    let mut opts = options();
    opts.temperature = Some(0.5);
    opts.max_tokens = Some(256);
    let events = collect(GoogleGenerativeAi.stream(&m, &ctx, Some(opts))).await;
    assert!(matches!(events.last(), Some(StreamEvent::Done { .. })));

    let request = captured.recv().await.expect("request captured");
    let body = request.body_json();
    // SDK tContent(string) → {role: "user", parts: [{text}]}.
    assert_eq!(
        body["systemInstruction"],
        json!({"parts": [{"text": "Be brief."}], "role": "user"})
    );
    assert_eq!(
        body["generationConfig"],
        json!({"temperature": 0.5, "maxOutputTokens": 256})
    );
}

// ---------------------------------------------------------------------------
// thinking 流 + thoughtSignature 保留
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_google_thinking_stream_retains_thought_signature() {
    let (base_url, _captured) = serve(vec![(200, GOOGLE_THINKING_SSE)]).await;
    let m = model(&base_url);
    let events =
        collect(GoogleGenerativeAi.stream(&m, &context(vec![user_text("hi")]), Some(options())))
            .await;

    assert_eq!(
        event_kinds(&events),
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
    let StreamEvent::Done { message, .. } = events.last().expect("done") else {
        panic!("expected done event");
    };
    let pir_ai::types::AssistantContent::Thinking(thinking) = &message.content[0] else {
        panic!("expected thinking block at index 0");
    };
    assert_eq!(thinking.thinking, "thinking... more");
    // The signature arrived only on the first delta; it is retained.
    assert_eq!(thinking.thinking_signature.as_deref(), Some("QUJDREVGRw=="));
    assert_eq!(message.usage.reasoning, Some(12));
}

// ---------------------------------------------------------------------------
// tool-call 流（无函数调用流式 + id 自增）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_google_tool_call_stream() {
    let (base_url, mut captured) = serve(vec![(200, GOOGLE_TOOL_CALL_SSE)]).await;
    let m = model(&base_url);
    let mut ctx = context(vec![user_text("hi")]);
    ctx.tools = Some(vec![make_tool(json!({
        "type": "object",
        "properties": {"command": {"type": "string"}},
        "required": ["command"],
    }))]);
    let events = collect(GoogleGenerativeAi.stream(&m, &ctx, Some(options()))).await;

    let request = captured.recv().await.expect("request captured");
    let body = request.body_json();
    assert_eq!(
        body["tools"][0]["functionDeclarations"][0]["name"],
        json!("test_tool")
    );
    assert!(body["tools"][0]["functionDeclarations"][0]
        .get("parametersJsonSchema")
        .is_some());
    // No toolChoice and no strict tools: no toolConfig.
    assert!(body.get("toolConfig").is_none());

    assert_eq!(
        event_kinds(&events),
        vec![
            "start",
            "text_start",
            "text_delta",
            "text_end",
            "toolcall_start",
            "toolcall_delta",
            "toolcall_end",
            "done"
        ]
    );
    // No function call streaming: a single delta carries the full arguments.
    let StreamEvent::ToolCallDelta { delta, .. } = &events[5] else {
        panic!("expected toolcall_delta");
    };
    assert_eq!(delta, "{\"command\":\"ls\"}");

    let StreamEvent::Done { reason, message } = events.last().expect("done") else {
        panic!("expected done event");
    };
    // A tool call overrides the STOP finish reason.
    assert_eq!(*reason, pir_ai::types::DoneReason::ToolUse);
    assert_eq!(message.stop_reason, StopReason::ToolUse);
    let pir_ai::types::AssistantContent::ToolCall(call) = &message.content[1] else {
        panic!("expected tool call at index 1");
    };
    assert_eq!(call.name, "bash");
    assert_eq!(call.arguments["command"], json!("ls"));
    // Unsigned upstream part id → generated `{name}_{ts}_{counter}` id.
    assert!(
        call.id.starts_with("bash_"),
        "generated id has the name prefix: {}",
        call.id
    );
    assert_eq!(call.thought_signature.as_deref(), Some("QUJDREVGRw=="));
}

#[tokio::test]
async fn test_google_tool_choice_any_sets_validated_mode_off() {
    let (base_url, mut captured) = serve(vec![(200, GOOGLE_TOOL_CALL_SSE)]).await;
    let m = model(&base_url);
    let mut ctx = context(vec![user_text("hi")]);
    ctx.tools = Some(vec![make_tool(json!({"type": "object", "properties": {}}))]);
    let events = collect(pir_ai::api::google_generative_ai::stream(
        &m,
        &ctx,
        pir_ai::api::google_generative_ai::GoogleOptions {
            stream: options(),
            tool_choice: Some(pir_ai::api::google_generative_ai::GoogleToolChoice::Any),
            thinking: None,
        },
    ))
    .await;
    assert!(matches!(events.last(), Some(StreamEvent::Done { .. })));

    let request = captured.recv().await.expect("request captured");
    let body = request.body_json();
    assert_eq!(
        body["toolConfig"],
        json!({"functionCallingConfig": {"mode": "ANY"}})
    );
}

// ---------------------------------------------------------------------------
// 错误流
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_google_http_error_surfaces_body() {
    let error_body =
        "{\"error\":{\"code\":400,\"message\":\"bad request\",\"status\":\"INVALID_ARGUMENT\"}}";
    let (base_url, _captured) = serve(vec![(400, error_body)]).await;
    let m = model(&base_url);
    let events =
        collect(GoogleGenerativeAi.stream(&m, &context(vec![user_text("hi")]), Some(options())))
            .await;
    assert_eq!(event_kinds(&events), vec!["error"]);
    let StreamEvent::Error { error, .. } = &events[0] else {
        panic!("expected error event");
    };
    assert_eq!(error.stop_reason, StopReason::Error);
    // SDK `throwErrorIfNotOK`: JSON bodies are stringified verbatim into the
    // error message.
    assert_eq!(error.error_message.as_deref(), Some(error_body));
}

#[tokio::test]
async fn test_google_in_stream_error_chunk() {
    // A raw (non-SSE) JSON error body on a 200 response: the SDK's
    // processStreamResponse chunk probe aborts with `got status: ...`.
    let (base_url, _captured) = serve(vec![(
        200,
        "{\"error\":{\"code\":429,\"message\":\"quota\",\"status\":\"RESOURCE_EXHAUSTED\"}}",
    )])
    .await;
    let m = model(&base_url);
    let events =
        collect(GoogleGenerativeAi.stream(&m, &context(vec![user_text("hi")]), Some(options())))
            .await;
    assert_eq!(event_kinds(&events), vec!["start", "error"]);
    let StreamEvent::Error { error, .. } = &events[1] else {
        panic!("expected error event");
    };
    let message = error.error_message.as_deref().expect("error message");
    assert!(
        message.starts_with("got status: RESOURCE_EXHAUSTED. "),
        "{message}"
    );
}

#[tokio::test]
async fn test_google_stream_without_finish_reason_is_error() {
    let (base_url, _captured) = serve(vec![(200, GOOGLE_NO_FINISH_SSE)]).await;
    let m = model(&base_url);
    let events =
        collect(GoogleGenerativeAi.stream(&m, &context(vec![user_text("hi")]), Some(options())))
            .await;
    assert_eq!(
        event_kinds(&events),
        vec!["start", "text_start", "text_delta", "text_end", "error"]
    );
    let StreamEvent::Error { error, .. } = events.last().expect("error") else {
        panic!("expected error event");
    };
    assert_eq!(
        error.error_message.as_deref(),
        Some("Google stream ended without a finish reason")
    );
}

#[tokio::test]
async fn test_google_stream_simple_without_api_key_errors() {
    let m = model("http://127.0.0.1:1");
    let events = collect(stream_simple(
        &m,
        &context(vec![user_text("hi")]),
        Some(SimpleStreamOptions::default()),
    ))
    .await;
    assert_eq!(event_kinds(&events), vec!["error"]);
    let StreamEvent::Error { error, .. } = &events[0] else {
        panic!("expected error event");
    };
    assert_eq!(
        error.error_message.as_deref(),
        Some("No API key for provider: google")
    );
}

// ---------------------------------------------------------------------------
// 锚点：thinking 分流与 budget 档位表（wire 断言，覆盖
// google-thinking-disable.test.ts 的意图——上游为 env 门控的 live E2E）
// ---------------------------------------------------------------------------

async fn assert_thinking_config(
    model_id: &str,
    reasoning: Option<ThinkingLevel>,
    budgets: Option<ThinkingBudgets>,
    expected: Value,
) {
    let (base_url, mut captured) = serve(vec![(200, GOOGLE_TEXT_SSE)]).await;
    let m = model_with_id(model_id, &base_url, json!({}));
    let events = collect(stream_simple(
        &m,
        &context(vec![user_text("hi")]),
        Some(simple_options(reasoning, budgets)),
    ))
    .await;
    assert!(
        matches!(events.last(), Some(StreamEvent::Done { .. })),
        "{model_id}: stream completes"
    );
    let request = captured.recv().await.expect("request captured");
    let body = request.body_json();
    assert_eq!(
        body["generationConfig"]["thinkingConfig"], expected,
        "{model_id}: thinkingConfig"
    );
}

#[tokio::test]
async fn test_budget_table_gemini_2_5_pro() {
    for (level, budget) in [
        (ThinkingLevel::Minimal, 128),
        (ThinkingLevel::Low, 2048),
        (ThinkingLevel::Medium, 8192),
        (ThinkingLevel::High, 32768),
    ] {
        assert_thinking_config(
            "gemini-2.5-pro",
            Some(level),
            None,
            json!({"includeThoughts": true, "thinkingBudget": budget}),
        )
        .await;
    }
}

#[tokio::test]
async fn test_budget_table_gemini_2_5_flash_lite() {
    for (level, budget) in [
        (ThinkingLevel::Minimal, 512),
        (ThinkingLevel::Low, 2048),
        (ThinkingLevel::Medium, 8192),
        (ThinkingLevel::High, 24576),
    ] {
        assert_thinking_config(
            "gemini-2.5-flash-lite",
            Some(level),
            None,
            json!({"includeThoughts": true, "thinkingBudget": budget}),
        )
        .await;
    }
}

#[tokio::test]
async fn test_budget_table_gemini_2_5_flash() {
    for (level, budget) in [
        (ThinkingLevel::Minimal, 128),
        (ThinkingLevel::Low, 2048),
        (ThinkingLevel::Medium, 8192),
        (ThinkingLevel::High, 24576),
    ] {
        assert_thinking_config(
            "gemini-2.5-flash",
            Some(level),
            None,
            json!({"includeThoughts": true, "thinkingBudget": budget}),
        )
        .await;
    }
}

#[tokio::test]
async fn test_budget_dynamic_minus_one_for_other_models() {
    assert_thinking_config(
        "gemini-2.0-flash",
        Some(ThinkingLevel::High),
        None,
        json!({"includeThoughts": true, "thinkingBudget": -1}),
    )
    .await;
}

#[tokio::test]
async fn test_custom_thinking_budgets_win() {
    let budgets = ThinkingBudgets {
        minimal: None,
        low: None,
        medium: Some(555),
        high: None,
    };
    assert_thinking_config(
        "gemini-2.5-pro",
        Some(ThinkingLevel::Medium),
        Some(budgets),
        json!({"includeThoughts": true, "thinkingBudget": 555}),
    )
    .await;
}

#[tokio::test]
async fn test_thinking_level_split_gemini_3_and_gemma_4() {
    // Gemini 3 Pro: minimal/low → LOW, medium/high → HIGH.
    assert_thinking_config(
        "gemini-3-pro-preview",
        Some(ThinkingLevel::Low),
        None,
        json!({"includeThoughts": true, "thinkingLevel": "LOW"}),
    )
    .await;
    assert_thinking_config(
        "gemini-3.1-pro-preview",
        Some(ThinkingLevel::Medium),
        None,
        json!({"includeThoughts": true, "thinkingLevel": "HIGH"}),
    )
    .await;
    // Gemini 3 Flash: full level range.
    assert_thinking_config(
        "gemini-3-flash-preview",
        Some(ThinkingLevel::Minimal),
        None,
        json!({"includeThoughts": true, "thinkingLevel": "MINIMAL"}),
    )
    .await;
    assert_thinking_config(
        "gemini-flash-latest",
        Some(ThinkingLevel::Medium),
        None,
        json!({"includeThoughts": true, "thinkingLevel": "MEDIUM"}),
    )
    .await;
    // Gemma 4: minimal/low → MINIMAL, medium/high → HIGH.
    assert_thinking_config(
        "gemma-4-27b-it",
        Some(ThinkingLevel::Low),
        None,
        json!({"includeThoughts": true, "thinkingLevel": "MINIMAL"}),
    )
    .await;
    assert_thinking_config(
        "gemma-4-27b-it",
        Some(ThinkingLevel::High),
        None,
        json!({"includeThoughts": true, "thinkingLevel": "HIGH"}),
    )
    .await;
}

#[tokio::test]
async fn test_thinking_disable_configs() {
    // Gemini 2.x disables via thinkingBudget = 0.
    assert_thinking_config("gemini-2.5-flash", None, None, json!({"thinkingBudget": 0})).await;
    // Gemini 3 / Gemma 4 cannot disable thinking: lowest level, no
    // includeThoughts.
    assert_thinking_config(
        "gemini-3-pro-preview",
        None,
        None,
        json!({"thinkingLevel": "LOW"}),
    )
    .await;
    assert_thinking_config(
        "gemini-3-flash-preview",
        None,
        None,
        json!({"thinkingLevel": "MINIMAL"}),
    )
    .await;
    assert_thinking_config(
        "gemini-flash-lite-latest",
        None,
        None,
        json!({"thinkingLevel": "MINIMAL"}),
    )
    .await;
    assert_thinking_config(
        "gemma-4-27b-it",
        None,
        None,
        json!({"thinkingLevel": "MINIMAL"}),
    )
    .await;
}

// ---------------------------------------------------------------------------
// 上游测试意图移植：google-thinking-signature.test.ts
// ---------------------------------------------------------------------------

#[test]
fn test_is_thinking_part_thought_true_only() {
    assert!(is_thinking_part(Some(true)));
    // thoughtSignature alone never marks thinking content.
    assert!(!is_thinking_part(None));
    assert!(!is_thinking_part(Some(false)));
}

#[test]
fn test_retain_thought_signature() {
    let first = retain_thought_signature(None, Some("sig-1"));
    assert_eq!(first.as_deref(), Some("sig-1"));
    let second = retain_thought_signature(first.as_deref(), None);
    assert_eq!(second.as_deref(), Some("sig-1"));
    let third = retain_thought_signature(second.as_deref(), Some(""));
    assert_eq!(third.as_deref(), Some("sig-1"));
    // A new non-empty signature replaces the old one.
    let updated = retain_thought_signature(third.as_deref(), Some("sig-2"));
    assert_eq!(updated.as_deref(), Some("sig-2"));
}

// ---------------------------------------------------------------------------
// 上游测试意图移植：google-shared-convert-tools.test.ts
// ---------------------------------------------------------------------------

#[test]
fn test_convert_tools_strips_meta_keys_with_use_parameters() {
    let tools = vec![make_tool(json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "$id": "urn:bash-tool",
        "$comment": "A bash tool for demonstration",
        "$defs": {"commandDef": {"type": "string"}},
        "definitions": {"legacyDef": {"type": "number"}},
        "type": "object",
        "properties": {"command": {"type": "string"}},
        "required": ["command"],
    }))];
    let result = convert_tools(&tools, true).expect("converted");
    let decl = &result[0]["functionDeclarations"][0];
    assert_eq!(
        decl["parameters"],
        json!({
            "type": "object",
            "properties": {"command": {"type": "string"}},
            "required": ["command"],
        })
    );
}

#[test]
fn test_convert_tools_strips_nested_meta_keys_recursively() {
    let tools = vec![make_tool(json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": {
            "deep": {
                "$schema": "http://json-schema.org/draft-07/schema#",
                "$id": "urn:nested",
                "type": "string",
            },
        },
    }))];
    let result = convert_tools(&tools, true).expect("converted");
    assert_eq!(
        result[0]["functionDeclarations"][0]["parameters"],
        json!({
            "type": "object",
            "properties": {"deep": {"type": "string"}},
        })
    );
}

#[test]
fn test_convert_tools_preserves_ref_while_stripping_meta_keys() {
    let tools = vec![make_tool(json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": {
            "refProp": {"$ref": "#/$defs/someDef", "type": "string"},
        },
    }))];
    let result = convert_tools(&tools, true).expect("converted");
    assert_eq!(
        result[0]["functionDeclarations"][0]["parameters"],
        json!({
            "type": "object",
            "properties": {
                "refProp": {"$ref": "#/$defs/someDef", "type": "string"},
            },
        })
    );
}

#[test]
fn test_convert_tools_does_not_mutate_original_parameters() {
    let original = json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": {"command": {"type": "string"}},
        "required": ["command"],
    });
    let tools = vec![make_tool(original.clone())];
    let _ = convert_tools(&tools, true);
    assert_eq!(tools[0].parameters, original);
}

#[test]
fn test_convert_tools_preserves_schema_in_parameters_json_schema() {
    let tools = vec![make_tool(json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": {"command": {"type": "string"}},
        "required": ["command"],
    }))];
    let result = convert_tools(&tools, false).expect("converted");
    assert_eq!(
        result[0]["functionDeclarations"][0]["parametersJsonSchema"],
        json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "properties": {"command": {"type": "string"}},
            "required": ["command"],
        })
    );
}

#[test]
fn test_convert_tools_handles_tools_without_schema_meta() {
    let tools = vec![make_tool(json!({
        "type": "object",
        "properties": {"path": {"type": "string"}},
        "required": ["path"],
    }))];
    let result = convert_tools(&tools, true).expect("converted");
    assert_eq!(
        result[0]["functionDeclarations"][0]["parameters"],
        json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"],
        })
    );
}

#[test]
fn test_convert_tools_empty_list_is_none() {
    assert!(convert_tools(&[], false).is_none());
    assert!(convert_tools(&[], true).is_none());
}

#[test]
fn test_strict_tools_use_validated_mode_on_gemini_3() {
    let mut tool = make_tool(json!({"type": "object", "properties": {}}));
    tool.constrained_sampling = serde_json::from_value(json!({
        "type": "json_schema", "strict": "require"
    }))
    .expect("constrainedSampling");

    assert!(supports_google_strict_tool_sampling(
        "gemini-3.1-pro-preview"
    ));
    assert!(!supports_google_strict_tool_sampling("gemini-2.5-pro"));
    assert_eq!(
        resolve_google_function_calling_mode(&[tool.clone()], None, true).expect("mode"),
        Some("VALIDATED")
    );
    let error =
        resolve_google_function_calling_mode(&[tool], None, false).expect_err("strict unsupported");
    assert!(
        error.contains("requires JSON-schema constrained sampling"),
        "{error}"
    );
}

// ---------------------------------------------------------------------------
// 上游测试意图移植：google-shared-gemini3-unsigned-tool-call.test.ts
// ---------------------------------------------------------------------------

fn gemini3_model(id: &str) -> Model {
    model_with_id(id, "https://example.com", json!({}))
}

fn tool_call_context(model: &Model, thought_signature: Option<&str>) -> Context {
    let mut call1 = json!({
        "type": "toolCall", "id": "call_1", "name": "bash",
        "arguments": {"command": "echo hi"},
    });
    if let Some(signature) = thought_signature {
        call1["thoughtSignature"] = json!(signature);
    }
    let assistant: Message = serde_json::from_value(json!({
        "role": "assistant",
        "content": [
            call1,
            {"type": "toolCall", "id": "call_2", "name": "bash", "arguments": {"command": "ls -la"}},
        ],
        "api": model.api, "provider": model.provider, "model": model.id,
        "usage": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 0,
                  "cost": {"input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0, "total": 0.0}},
        "stopReason": "toolUse", "timestamp": 0
    }))
    .expect("assistant");
    context(vec![user_text("Hi"), assistant])
}

fn function_call_parts(contents: &[Value]) -> Vec<&Value> {
    let model_turn = contents
        .iter()
        .find(|content| content["role"] == json!("model"))
        .expect("model turn");
    model_turn["parts"]
        .as_array()
        .expect("parts")
        .iter()
        .filter(|part| part.get("functionCall").is_some())
        .collect()
}

#[test]
fn test_convert_messages_unsigned_tool_calls_have_no_signature() {
    // Different assistant model id → not same provider/model → no signature.
    let model = gemini3_model("gemini-3-pro-preview");
    let mut ctx_model = model.clone();
    ctx_model.id = "other-model".to_owned();
    let contents = convert_messages(&model, &tool_call_context(&ctx_model, None));

    let parts = function_call_parts(&contents);
    assert_eq!(parts.len(), 2);
    assert!(parts[0].get("thoughtSignature").is_none());
    assert!(parts[1].get("thoughtSignature").is_none());
    let serialized = serde_json::to_string(&contents).expect("json");
    assert!(!serialized.contains("skip_thought_signature_validator"));
}

#[test]
fn test_convert_messages_preserves_valid_signature_same_provider_model() {
    let model = gemini3_model("gemini-3-pro-preview");
    let valid_sig = "AAAAAAAAAAAAAAAAAAAAAA==";
    let contents = convert_messages(&model, &tool_call_context(&model, Some(valid_sig)));

    let parts = function_call_parts(&contents);
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0]["thoughtSignature"], json!(valid_sig));
    assert!(parts[1].get("thoughtSignature").is_none());
}

#[test]
fn test_convert_messages_drops_signature_for_other_model() {
    let model = gemini3_model("gemini-2.5-flash");
    let mut ctx_model = model.clone();
    ctx_model.id = "other-model".to_owned();
    let contents = convert_messages(
        &model,
        &tool_call_context(&ctx_model, Some("AAAAAAAAAAAAAAAAAAAAAA==")),
    );
    let parts = function_call_parts(&contents);
    assert!(!parts.is_empty());
    assert!(parts
        .iter()
        .all(|part| part.get("thoughtSignature").is_none()));
}

// ---------------------------------------------------------------------------
// 上游测试意图移植：google-shared-image-tool-result-routing.test.ts
// ---------------------------------------------------------------------------

fn image_routing_context(model: &Model) -> Context {
    let assistant: Message = serde_json::from_value(json!({
        "role": "assistant",
        "content": [
            {"type": "toolCall", "id": "call_a", "name": "read", "arguments": {"path": "a.txt"}},
            {"type": "toolCall", "id": "call_img", "name": "read", "arguments": {"path": "image.png"}},
            {"type": "toolCall", "id": "call_b", "name": "read", "arguments": {"path": "b.txt"}},
        ],
        "api": model.api, "provider": model.provider, "model": model.id,
        "usage": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 0,
                  "cost": {"input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0, "total": 0.0}},
        "stopReason": "toolUse", "timestamp": 0
    }))
    .expect("assistant");
    let tool_result = |id: &str, content: Value| -> Message {
        serde_json::from_value(json!({
            "role": "toolResult", "toolCallId": id, "toolName": "read",
            "content": content, "isError": false, "timestamp": 0
        }))
        .expect("toolResult")
    };
    context(vec![
        user_text("read the files"),
        assistant,
        tool_result("call_a", json!([{"type": "text", "text": "alpha text"}])),
        tool_result(
            "call_img",
            json!([{"type": "image", "data": "abc", "mimeType": "image/png"}]),
        ),
        tool_result("call_b", json!([{"type": "text", "text": "beta text"}])),
    ])
}

#[test]
fn test_image_tool_result_separate_turn_for_gemini_2_x() {
    let model = model_with_id(
        "gemini-2.5-flash",
        "https://example.com",
        json!({"input": ["text", "image"]}),
    );
    let contents = convert_messages(&model, &image_routing_context(&model));

    assert_eq!(contents.len(), 5);
    // All function responses merge into one user turn.
    let merged = contents[2]["parts"].as_array().expect("parts");
    assert!(merged
        .iter()
        .all(|part| part.get("functionResponse").is_some()));
    // Images go in a separate synthetic user turn for Gemini < 3.
    assert_eq!(contents[3]["role"], json!("user"));
    assert_eq!(contents[3]["parts"][0]["text"], json!("Tool result image:"));
    assert!(contents[3]["parts"][1].get("inlineData").is_some());
    assert!(contents[4]["parts"][0].get("functionResponse").is_some());
}

#[test]
fn test_image_tool_result_nested_for_gemini_3() {
    let model = model_with_id(
        "gemini-3-pro-preview",
        "https://example.com",
        json!({"input": ["text", "image"]}),
    );
    let contents = convert_messages(&model, &image_routing_context(&model));

    assert_eq!(contents.len(), 3);
    let tool_result_turn = contents[2]["parts"].as_array().expect("parts");
    assert_eq!(tool_result_turn.len(), 3);
    let image_response = &tool_result_turn[1]["functionResponse"];
    assert!(image_response.is_object());
    let nested = image_response["parts"].as_array().expect("nested parts");
    assert_eq!(nested.len(), 1);
    assert!(nested[0].get("inlineData").is_some());
}

// ---------------------------------------------------------------------------
// 其余锚点：stop 映射 / requiresToolCallId
// ---------------------------------------------------------------------------

#[test]
fn test_map_stop_reason() {
    assert_eq!(map_stop_reason("STOP").expect("stop"), StopReason::Stop);
    assert_eq!(
        map_stop_reason("MAX_TOKENS").expect("length"),
        StopReason::Length
    );
    for reason in ["SAFETY", "RECITATION", "MALFORMED_FUNCTION_CALL", "OTHER"] {
        assert_eq!(map_stop_reason(reason).expect("error"), StopReason::Error);
    }
    // Unknown reasons are an error (upstream's `never` check throws).
    assert_eq!(
        map_stop_reason("SOME_FUTURE_REASON"),
        Err("Unhandled stop reason: SOME_FUTURE_REASON".to_owned())
    );
    assert_eq!(map_stop_reason_string("STOP"), StopReason::Stop);
    assert_eq!(map_stop_reason_string("MAX_TOKENS"), StopReason::Length);
    assert_eq!(map_stop_reason_string("SAFETY"), StopReason::Error);
}

#[test]
fn test_requires_tool_call_id() {
    assert!(requires_tool_call_id("claude-opus-4-1"));
    assert!(requires_tool_call_id("gpt-oss-120b"));
    assert!(!requires_tool_call_id("gemini-3-pro-preview"));
}
