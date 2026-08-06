//! Contract tests for the azure-openai-responses adapter: drive `stream()` /
//! `stream_simple()` over a scripted local HTTP server with recorded SSE
//! streams, and assert both sides of the contract — the request shape
//! (method / path + `api-version` query / key headers / body JSON) and the
//! emitted `StreamEvent` sequence. Mirrors `contract_adapters.rs`; the SSE
//! payloads are recorded in the upstream OpenAI Responses wire format.
//!
//! Base-URL normalization (three Azure host suffixes, path collapse to
//! `/openai/v1`), deployment-name map and API-version defaults are asserted
//! against the adapter's exported resolution helpers — the upstream vitest
//! suite captures the SDK client constructor arguments; the pir adapter has
//! no SDK client, so the resolution functions are the observable equivalent
//! (azure-openai-base-url.test.ts intent). The reasoning-replay cases mirror
//! azure-openai-responses-reasoning-replay.test.ts at the adapter level.

use std::time::Duration;

use futures::StreamExt;
use pir_ai::api::azure_openai_responses::{
    self, normalize_azure_base_url, resolve_azure_config, AzureOpenAIResponsesOptions,
    AzureOpenAiResponses, AZURE_TOOL_CALL_PROVIDERS,
};
use pir_ai::api::openai_responses_shared::{
    convert_responses_messages, ConvertResponsesMessagesOptions,
};
use pir_ai::models::ProviderStreams;
use pir_ai::types::{
    ApiKind, Context, Message, Model, ProviderEnv, SimpleStreamOptions, StopReason, StreamEvent,
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

fn model(base_url: &str, extra: Value) -> Model {
    let mut value = json!({
        "id": "gpt-4o-mini", "name": "GPT-4o mini",
        "api": ApiKind::AZURE_OPENAI_RESPONSES, "provider": "azure-openai-responses",
        "baseUrl": base_url, "reasoning": false, "input": ["text"],
        "cost": {"input": 1.0, "output": 2.0, "cacheRead": 0.5, "cacheWrite": 1.0},
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
// Recorded SSE streams (OpenAI Responses wire format)
// ---------------------------------------------------------------------------

const TEXT_SSE: &str = concat!(
    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"status\":\"in_progress\"}}\n",
    "\n",
    "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\",\"content\":[]}}\n",
    "\n",
    "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"Hello\"}\n",
    "\n",
    "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\",\"status\":\"completed\",\"content\":[{\"type\":\"output_text\",\"text\":\"Hello\",\"annotations\":[]}]}}\n",
    "\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"usage\":{\"input_tokens\":10,\"output_tokens\":5,\"total_tokens\":15,\"input_tokens_details\":{\"cached_tokens\":2},\"output_tokens_details\":{\"reasoning_tokens\":0}}}}\n",
    "\n",
);

/// Reasoning stream where `response.output_item.done` carries the
/// `encrypted_content`; `response.completed` repeats the item with a
/// different value (azure-openai-responses-reasoning-replay.test.ts case 1).
const REASONING_DONE_SSE: &str = concat!(
    "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"id\":\"rs_done\",\"summary\":[]}}\n",
    "\n",
    "data: {\"type\":\"response.reasoning_text.delta\",\"output_index\":0,\"delta\":\"hmm\"}\n",
    "\n",
    "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"id\":\"rs_done\",\"summary\":[],\"encrypted_content\":\"from-output-item-done\"}}\n",
    "\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_r\",\"status\":\"completed\",\"output\":[{\"type\":\"reasoning\",\"id\":\"rs_done\",\"summary\":[],\"encrypted_content\":\"from-response-completed\"}]}}\n",
    "\n",
);

/// Reasoning stream where `encrypted_content` only appears in the terminal
/// `response.completed` output (reasoning-replay test case 2).
const REASONING_COMPLETED_SSE: &str = concat!(
    "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"id\":\"rs_missing\",\"summary\":[]}}\n",
    "\n",
    "data: {\"type\":\"response.reasoning_text.delta\",\"output_index\":0,\"delta\":\"hmm\"}\n",
    "\n",
    "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"id\":\"rs_missing\",\"summary\":[]}}\n",
    "\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_r\",\"status\":\"completed\",\"output\":[{\"type\":\"reasoning\",\"id\":\"rs_missing\",\"summary\":[],\"encrypted_content\":\"from-response-completed\"}]}}\n",
    "\n",
);

const TOOL_CALL_SSE: &str = concat!(
    "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"id\":\"fc_1\",\"name\":\"bash\",\"arguments\":\"\"}}\n",
    "\n",
    "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"{\\\"cmd\\\":\\\"ls\\\"}\"}\n",
    "\n",
    "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"id\":\"fc_1\",\"name\":\"bash\",\"arguments\":\"{\\\"cmd\\\":\\\"ls\\\"}\"}}\n",
    "\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_t\",\"status\":\"completed\"}}\n",
    "\n",
);

// ---------------------------------------------------------------------------
// Contract tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_azure_openai_responses_contract() {
    let (base_url, mut captured) = serve(vec![(200, TEXT_SSE)]).await;
    let m = model(&base_url, json!({}));
    let events =
        collect(AzureOpenAiResponses.stream(&m, &context(vec![user_text("hi")]), Some(options())))
            .await;

    let request = captured.recv().await.expect("request captured");
    assert_eq!(request.method, "POST");
    // SDK-verified wire shape: {baseUrl}/responses?api-version=v1 — the
    // deployment travels in the body, not the path (openai@6.26.0).
    assert_eq!(request.path, "/responses?api-version=v1");
    // Azure auth header; no Authorization: Bearer.
    assert_eq!(request.header("api-key"), Some("test-key"));
    assert!(request.header("authorization").is_none());
    let body = request.body_json();
    // No deployment map / option: the model id is the deployment name.
    assert_eq!(body["model"], json!("gpt-4o-mini"));
    assert_eq!(body["stream"], json!(true));
    assert_eq!(body["store"], json!(false));

    assert_eq!(
        event_kinds(&events),
        vec!["start", "text_start", "text_delta", "text_end", "done"]
    );
    let StreamEvent::Done { reason, message } = &events[4] else {
        panic!("expected done event");
    };
    assert_eq!(*reason, pir_ai::types::DoneReason::Stop);
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(message.response_id.as_deref(), Some("resp_1"));
    assert_eq!(message.usage.input, 8);
    assert_eq!(message.usage.cache_read, 2);
}

#[tokio::test]
async fn test_azure_reasoning_replay_preserves_output_item_done_encrypted_content() {
    let (base_url, mut captured) = serve(vec![(200, REASONING_DONE_SSE)]).await;
    let m = model(&base_url, json!({"reasoning": true}));
    // stream_simple with reasoning → effort/summary params + encrypted
    // content include (azure-openai-responses.ts:306-316).
    let events = collect(AzureOpenAiResponses.stream_simple(
        &m,
        &context(vec![user_text("hi")]),
        Some(SimpleStreamOptions {
            stream: options(),
            reasoning: Some(ThinkingLevel::High),
            thinking_budgets: None,
        }),
    ))
    .await;

    let request = captured.recv().await.expect("request captured");
    let body = request.body_json();
    assert_eq!(
        body["reasoning"],
        json!({"effort": "high", "summary": "auto"})
    );
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));

    assert_eq!(
        event_kinds(&events),
        vec![
            "start",
            "thinking_start",
            "thinking_delta",
            "thinking_end",
            "done"
        ]
    );
    let StreamEvent::Done { message, .. } = &events[4] else {
        panic!("expected done event");
    };

    // Replay the assistant turn: the reasoning item keeps the
    // output_item.done encrypted_content (not the response.completed one).
    let replay = Context {
        system_prompt: None,
        messages: vec![
            user_text("first"),
            Message::Assistant(message.clone()),
            user_text("follow-up"),
        ],
        tools: None,
    };
    let input = convert_responses_messages(
        &m,
        &replay,
        &AZURE_TOOL_CALL_PROVIDERS,
        &ConvertResponsesMessagesOptions::default(),
    )
    .expect("convert");
    let reasoning = input
        .iter()
        .find(|item| item["type"] == json!("reasoning"))
        .expect("replayed reasoning item");
    assert_eq!(reasoning["id"], json!("rs_done"));
    assert_eq!(
        reasoning["encrypted_content"],
        json!("from-output-item-done")
    );
}

#[tokio::test]
async fn test_azure_reasoning_replay_fills_encrypted_content_from_completed() {
    let (base_url, _captured) = serve(vec![(200, REASONING_COMPLETED_SSE)]).await;
    let m = model(&base_url, json!({"reasoning": true}));
    let events =
        collect(AzureOpenAiResponses.stream(&m, &context(vec![user_text("hi")]), Some(options())))
            .await;
    let StreamEvent::Done { message, .. } = events.last().expect("done") else {
        panic!("expected done event");
    };

    // output_item.done omitted encrypted_content → filled from
    // response.completed.
    let replay = Context {
        system_prompt: None,
        messages: vec![
            user_text("first"),
            Message::Assistant(message.clone()),
            user_text("follow-up"),
        ],
        tools: None,
    };
    let input = convert_responses_messages(
        &m,
        &replay,
        &AZURE_TOOL_CALL_PROVIDERS,
        &ConvertResponsesMessagesOptions::default(),
    )
    .expect("convert");
    let reasoning = input
        .iter()
        .find(|item| item["type"] == json!("reasoning"))
        .expect("replayed reasoning item");
    assert_eq!(reasoning["id"], json!("rs_missing"));
    assert_eq!(
        reasoning["encrypted_content"],
        json!("from-response-completed")
    );
}

#[tokio::test]
async fn test_azure_tool_call_stream() {
    let (base_url, mut captured) = serve(vec![(200, TOOL_CALL_SSE)]).await;
    let m = model(&base_url, json!({}));
    let tool: pir_ai::types::Tool = serde_json::from_value(json!({
        "name": "bash", "description": "Run a command",
        "parameters": {"type": "object", "properties": {"cmd": {"type": "string"}}, "required": ["cmd"]}
    }))
    .expect("tool");
    let ctx = Context {
        system_prompt: None,
        messages: vec![user_text("hi")],
        tools: Some(vec![tool]),
    };
    let events = collect(AzureOpenAiResponses.stream(&m, &ctx, Some(options()))).await;

    let request = captured.recv().await.expect("request captured");
    let body = request.body_json();
    // Azure default supportsStrictMode is true (unlike openai-responses):
    // the strict key is present on function tools.
    assert_eq!(body["tools"][0]["type"], json!("function"));
    assert_eq!(body["tools"][0]["name"], json!("bash"));
    assert_eq!(body["tools"][0]["strict"], json!(false));

    assert_eq!(
        event_kinds(&events),
        vec![
            "start",
            "toolcall_start",
            "toolcall_delta",
            "toolcall_end",
            "done"
        ]
    );
    let StreamEvent::Done { reason, message } = events.last().expect("done") else {
        panic!("expected done event");
    };
    assert_eq!(*reason, pir_ai::types::DoneReason::ToolUse);
    assert_eq!(message.stop_reason, StopReason::ToolUse);
    let pir_ai::types::AssistantContent::ToolCall(call) = &message.content[0] else {
        panic!("expected tool call block");
    };
    // Responses composite id form (azure-openai-responses is in
    // AZURE_TOOL_CALL_PROVIDERS).
    assert_eq!(call.id, "call_1|fc_1");
    assert_eq!(call.name, "bash");
    assert_eq!(
        serde_json::Value::Object(call.arguments.clone()),
        json!({"cmd": "ls"})
    );
}

#[tokio::test]
async fn test_azure_error_stream() {
    let error_body = "{\"error\":{\"message\":\"Deployment not found\"}}";
    let (base_url, mut captured) = serve(vec![(400, error_body)]).await;
    let m = model(&base_url, json!({}));
    let events =
        collect(AzureOpenAiResponses.stream(&m, &context(vec![user_text("hi")]), Some(options())))
            .await;

    captured.recv().await.expect("request captured");
    assert_eq!(event_kinds(&events), vec!["error"]);
    let StreamEvent::Error { reason, error } = &events[0] else {
        panic!("expected error event");
    };
    assert_eq!(*reason, pir_ai::types::ErrorReason::Error);
    assert_eq!(error.stop_reason, StopReason::Error);
    // formatAzureOpenAIError: prefix + status + body
    // (azure-openai-responses.ts:51-53).
    assert_eq!(
        error.error_message,
        Some(format!("Azure OpenAI API error (400): {error_body}"))
    );
}

#[tokio::test]
async fn test_azure_invalid_base_url_is_stream_error() {
    // azure-openai-base-url.test.ts "throws on invalid URLs" intent.
    let m = model("not-a-url", json!({}));
    let events =
        collect(AzureOpenAiResponses.stream(&m, &context(vec![user_text("hi")]), Some(options())))
            .await;
    assert_eq!(event_kinds(&events), vec!["error"]);
    let StreamEvent::Error { error, .. } = &events[0] else {
        panic!("expected error event");
    };
    assert_eq!(error.stop_reason, StopReason::Error);
    assert!(
        error
            .error_message
            .as_deref()
            .expect("error message")
            .contains("Invalid Azure OpenAI base URL"),
        "unexpected message: {:?}",
        error.error_message
    );
}

#[tokio::test]
async fn test_azure_deployment_name_resolution_over_wire() {
    // Deployment map from the scoped env override keys by model id.
    let (base_url, mut captured) = serve(vec![(200, TEXT_SSE)]).await;
    let m = model(&base_url, json!({}));
    let env: ProviderEnv = [(
        "AZURE_OPENAI_DEPLOYMENT_NAME_MAP".to_owned(),
        "gpt-4o-mini=mapped-deployment".to_owned(),
    )]
    .into();
    let opts = StreamOptions {
        env: Some(env),
        ..options()
    };
    let events =
        collect(AzureOpenAiResponses.stream(&m, &context(vec![user_text("hi")]), Some(opts))).await;
    let request = captured.recv().await.expect("request captured");
    assert_eq!(request.body_json()["model"], json!("mapped-deployment"));
    assert!(matches!(events.last(), Some(StreamEvent::Done { .. })));

    // The explicit azureDeploymentName option wins over the env map.
    let (base_url, mut captured) = serve(vec![(200, TEXT_SSE)]).await;
    let m = model(&base_url, json!({}));
    let env: ProviderEnv = [(
        "AZURE_OPENAI_DEPLOYMENT_NAME_MAP".to_owned(),
        "gpt-4o-mini=mapped-deployment".to_owned(),
    )]
    .into();
    let events = collect(azure_openai_responses::stream(
        &m,
        &context(vec![user_text("hi")]),
        AzureOpenAIResponsesOptions {
            stream: StreamOptions {
                env: Some(env),
                ..options()
            },
            azure_deployment_name: Some("explicit-deployment".to_owned()),
            ..AzureOpenAIResponsesOptions::default()
        },
    ))
    .await;
    let request = captured.recv().await.expect("request captured");
    assert_eq!(request.body_json()["model"], json!("explicit-deployment"));
    assert!(matches!(events.last(), Some(StreamEvent::Done { .. })));
}

#[tokio::test]
async fn test_azure_api_version_and_base_url_options_over_wire() {
    // Explicit azureBaseUrl + azureApiVersion reach the request URL.
    let (base_url, mut captured) = serve(vec![(200, TEXT_SSE)]).await;
    // model.baseUrl deliberately bogus: the option must win.
    let m = model("https://unused.example.com", json!({}));
    let events = collect(azure_openai_responses::stream(
        &m,
        &context(vec![user_text("hi")]),
        AzureOpenAIResponsesOptions {
            stream: options(),
            azure_base_url: Some(base_url),
            azure_api_version: Some("2024-12-01".to_owned()),
            ..AzureOpenAIResponsesOptions::default()
        },
    ))
    .await;
    let request = captured.recv().await.expect("request captured");
    assert_eq!(request.path, "/responses?api-version=2024-12-01");
    assert!(matches!(events.last(), Some(StreamEvent::Done { .. })));
}

// ---------------------------------------------------------------------------
// Resolution helpers (azure-openai-base-url.test.ts intent)
// ---------------------------------------------------------------------------

#[test]
fn test_azure_base_url_normalization_host_suffixes() {
    // All three Azure host suffixes collapse root endpoints to /openai/v1.
    for (input, expected) in [
        (
            "https://marc-quicktests-resource.cognitiveservices.azure.com",
            "https://marc-quicktests-resource.cognitiveservices.azure.com/openai/v1",
        ),
        (
            "https://marc-quicktests-resource.ai.azure.com",
            "https://marc-quicktests-resource.ai.azure.com/openai/v1",
        ),
        (
            "https://my-resource.openai.azure.com",
            "https://my-resource.openai.azure.com/openai/v1",
        ),
        // Path collapse variants on Azure hosts.
        (
            "https://my-resource.cognitiveservices.azure.com/openai",
            "https://my-resource.cognitiveservices.azure.com/openai/v1",
        ),
        (
            "https://my-resource.cognitiveservices.azure.com/openai/v1",
            "https://my-resource.cognitiveservices.azure.com/openai/v1",
        ),
        (
            "https://my-resource.services.ai.azure.com/openai/v1/responses",
            "https://my-resource.services.ai.azure.com/openai/v1",
        ),
        // Query stripped when normalizing Azure hosts; kept on proxies.
        (
            "https://my-resource.openai.azure.com/openai?api-version=2024-12-01",
            "https://my-resource.openai.azure.com/openai/v1",
        ),
        (
            "https://my-proxy.example.com/v1?custom=true",
            "https://my-proxy.example.com/v1?custom=true",
        ),
        (
            "https://my-proxy.example.com/v1",
            "https://my-proxy.example.com/v1",
        ),
    ] {
        assert_eq!(normalize_azure_base_url(input).expect("url"), expected);
    }
    assert!(normalize_azure_base_url("not-a-url").is_err());
}

#[test]
fn test_azure_config_resolution_defaults() {
    // Resource name builds the default URL; API version defaults to v1.
    let m = model("", json!({}));
    let options = AzureOpenAIResponsesOptions {
        azure_resource_name: Some("my-resource".to_owned()),
        ..AzureOpenAIResponsesOptions::default()
    };
    let config = resolve_azure_config(&m, Some(&options)).expect("config");
    assert_eq!(
        config.base_url,
        "https://my-resource.openai.azure.com/openai/v1"
    );
    assert_eq!(config.api_version, "v1");

    // AZURE_OPENAI_RESOURCE_NAME via scoped env does the same
    // (azure-openai-base-url.test.ts "builds correct default URL" intent).
    let env: ProviderEnv = [(
        "AZURE_OPENAI_RESOURCE_NAME".to_owned(),
        "my-resource".to_owned(),
    )]
    .into();
    let options = AzureOpenAIResponsesOptions {
        stream: StreamOptions {
            env: Some(env),
            ..StreamOptions::default()
        },
        ..AzureOpenAIResponsesOptions::default()
    };
    let config = resolve_azure_config(&m, Some(&options)).expect("config");
    assert_eq!(
        config.base_url,
        "https://my-resource.openai.azure.com/openai/v1"
    );

    // Nothing set anywhere → the upstream error message.
    let error = resolve_azure_config(&m, None).expect_err("missing");
    assert!(error.starts_with("Azure OpenAI base URL is required."));
}
