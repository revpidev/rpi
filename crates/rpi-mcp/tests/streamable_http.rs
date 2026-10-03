//! streamable HTTP transport tests, ported from
//! `packages/mcp/test/streamable-http.test.ts` @ a13d35a74.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, Response};
use rpi_mcp::protocol::McpError;
use rpi_mcp::{
    LATEST_PROTOCOL_VERSION, McpClient, McpClientOptions, McpRequestOptions,
    StreamableHttpTransport, StreamableHttpTransportOptions,
};
use serde_json::{Value, json};

#[derive(Clone)]
struct RecordedRequest {
    method: String,
    headers: HeaderMap,
    body: Option<Value>,
}

impl RecordedRequest {
    fn body_method(&self) -> Option<&str> {
        self.body.as_ref()?.get("method")?.as_str()
    }

    fn header(&self, name: &str) -> Option<String> {
        self.headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    }
}

struct TestResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl TestResponse {
    fn json(status: u16, body: Value, headers: &[(&str, &str)]) -> Self {
        Self {
            status,
            headers: headers
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
            body: serde_json::to_vec(&body).unwrap_or_default(),
        }
    }

    fn sse(status: u16, body: impl Into<String>, headers: &[(&str, &str)]) -> Self {
        let mut all: Vec<(String, String)> =
            vec![("content-type".to_owned(), "text/event-stream".to_owned())];
        all.extend(
            headers
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string())),
        );
        Self {
            status,
            headers: all,
            body: body.into().into_bytes(),
        }
    }
}

type HandlerResult = Pin<Box<dyn Future<Output = TestResponse> + Send>>;
type Handler = Arc<dyn Fn(RecordedRequest) -> HandlerResult + Send + Sync>;

struct FixtureState {
    handler: Handler,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

async fn handle(State(state): State<Arc<FixtureState>>, request: Request<Body>) -> Response<Body> {
    let method = request.method().to_string();
    let headers = request.headers().clone();
    let body = axum::body::to_bytes(request.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap_or_default();
    let recorded = RecordedRequest {
        method,
        headers,
        body: serde_json::from_slice(&body).ok(),
    };
    state.requests.lock().unwrap().push(recorded.clone());
    let response = (state.handler)(recorded).await;
    let mut builder = Response::builder().status(response.status);
    for (name, value) in &response.headers {
        builder = builder.header(name, value);
    }
    builder
        .body(Body::from(response.body))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

async fn start_server(handler: Handler) -> (String, Arc<Mutex<Vec<RecordedRequest>>>) {
    let state = Arc::new(FixtureState {
        handler,
        requests: Arc::new(Mutex::new(Vec::new())),
    });
    let app = Router::new().fallback(handle).with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}/mcp"), state.requests.clone())
}

/// `protocolHandler` (streamable-http.test.ts:38): initialize → JSON with a
/// session id, tools/list → JSON, other requests → SSE.
fn protocol_response(request: &RecordedRequest) -> TestResponse {
    if request.method == "GET" {
        return TestResponse {
            status: 405,
            headers: vec![],
            body: Vec::new(),
        };
    }
    if request.method == "DELETE" {
        return TestResponse {
            status: 200,
            headers: vec![],
            body: Vec::new(),
        };
    }
    let Some(message) = &request.body else {
        return TestResponse {
            status: 400,
            headers: vec![],
            body: Vec::new(),
        };
    };
    let Some(id) = message.get("id").cloned() else {
        return TestResponse {
            status: 202,
            headers: vec![],
            body: Vec::new(),
        };
    };
    match message.get("method").and_then(Value::as_str) {
        Some("initialize") => TestResponse::json(
            200,
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": LATEST_PROTOCOL_VERSION,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "http-fixture", "version": "1.0.0"},
                },
            }),
            &[
                ("content-type", "application/json"),
                ("mcp-session-id", "session-1"),
            ],
        ),
        Some("tools/list") => TestResponse::json(
            200,
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {"tools": [{"name": "echo", "inputSchema": {"type": "object"}}]},
            }),
            &[("content-type", "application/json")],
        ),
        _ => {
            let payload = json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {"content": [{"type": "text", "text": "hello"}]},
            });
            TestResponse::sse(200, format!("id: tool-result\ndata: {payload}\n\n"), &[])
        }
    }
}

fn protocol_handler() -> Handler {
    Arc::new(|request| Box::pin(async move { protocol_response(&request) }))
}

#[tokio::test]
async fn handles_json_and_sse_responses_with_session_and_protocol_headers() {
    let (url, requests) = start_server(protocol_handler()).await;
    let transport =
        Arc::new(StreamableHttpTransport::new(StreamableHttpTransportOptions::new(&url)).unwrap());
    let client = McpClient::new(McpClientOptions::new("http-test", "1.0.0"));
    client.connect(transport.clone()).await.unwrap();
    assert_eq!(transport.session_id().as_deref(), Some("session-1"));
    let tools = client
        .list_tools(McpRequestOptions::default())
        .await
        .unwrap();
    assert_eq!(tools[0].name, "echo");
    let result = client
        .call_tool(
            "echo",
            Some(json!({"text": "hello"})),
            McpRequestOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(result.content[0]["text"], "hello");
    client.close().await.unwrap();

    // The GET stream opened after initialization; the session and protocol
    // version headers ride post-handshake requests; close sends DELETE.
    for _ in 0..100 {
        let done = {
            let recorded = requests.lock().unwrap();
            recorded.iter().any(|request| request.method == "GET")
                && recorded.iter().any(|request| request.method == "DELETE")
        };
        if done {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let recorded = requests.lock().unwrap();
    let list = recorded
        .iter()
        .find(|request| request.body_method() == Some("tools/list"))
        .expect("tools/list recorded");
    assert_eq!(list.header("mcp-session-id").as_deref(), Some("session-1"));
    assert_eq!(
        list.header("mcp-protocol-version").as_deref(),
        Some(LATEST_PROTOCOL_VERSION)
    );
    assert!(recorded.iter().any(|request| request.method == "GET"));
    assert!(recorded.iter().any(|request| request.method == "DELETE"));
    assert!(
        recorded
            .iter()
            .all(|request| request.header("last-event-id").is_none())
    );
}

#[tokio::test]
async fn classifies_authentication_failures() {
    let (url, _requests) = start_server(Arc::new(|request| {
        Box::pin(async move {
            let _ = request;
            TestResponse {
                status: 401,
                headers: vec![(
                    "www-authenticate".to_owned(),
                    "Bearer resource_metadata=\"https://example.com/meta\"".to_owned(),
                )],
                body: b"login required".to_vec(),
            }
        })
    }))
    .await;
    let client = McpClient::new(McpClientOptions::new("http-test", "1.0.0"));
    let transport =
        Arc::new(StreamableHttpTransport::new(StreamableHttpTransportOptions::new(&url)).unwrap());
    let error = client.connect(transport).await.unwrap_err();
    assert!(matches!(error, McpError::AuthRequired), "{error:?}");
}

#[tokio::test]
async fn fails_only_the_request_whose_sse_stream_breaks() {
    let release_slow = Arc::new(tokio::sync::Notify::new());
    let (url, _requests) = start_server({
        let release_slow = release_slow.clone();
        Arc::new(move |request: RecordedRequest| -> HandlerResult {
            let release_slow = release_slow.clone();
            Box::pin(async move {
                if request.method != "POST" {
                    return protocol_response(&request);
                }
                let name = request
                    .body
                    .as_ref()
                    .and_then(|body| body.get("params"))
                    .and_then(|params| params.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                if name == "broken" {
                    return TestResponse::sse(200, "data: not json\n\n", &[]);
                }
                if name == "slow" {
                    release_slow.notified().await;
                }
                protocol_response(&request)
            })
        })
    })
    .await;
    let mut options = StreamableHttpTransportOptions::new(&url);
    options.open_get_stream = false;
    let transport = Arc::new(StreamableHttpTransport::new(options).unwrap());
    let client = McpClient::new(McpClientOptions::new("http-test", "1.0.0"));
    let errors: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let errors = errors.clone();
        client.on_error(Arc::new(move |error| {
            errors.lock().unwrap().push(error.to_string());
        }));
    }
    client.connect(transport).await.unwrap();

    let slow_client = client.clone();
    let slow = tokio::spawn(async move {
        slow_client
            .call_tool("slow", None, McpRequestOptions::default())
            .await
    });
    let error = client
        .call_tool("broken", None, McpRequestOptions::default())
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("MCP response stream failed"),
        "{error}"
    );
    release_slow.notify_waiters();
    let slow = tokio::time::timeout(std::time::Duration::from_secs(5), slow)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(slow.content[0]["text"], "hello");
    assert_eq!(errors.lock().unwrap().len(), 1);
    client.close().await.unwrap();
}

#[tokio::test]
async fn opens_the_get_stream_after_initialization() {
    let order: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let (url, _requests) = start_server({
        let order = order.clone();
        Arc::new(move |request: RecordedRequest| {
            let order = order.clone();
            Box::pin(async move {
                let name = request
                    .body
                    .as_ref()
                    .and_then(|body| body.get("method"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| request.method.clone());
                order.lock().unwrap().push(name);
                protocol_response(&request)
            })
        })
    })
    .await;
    let transport =
        Arc::new(StreamableHttpTransport::new(StreamableHttpTransportOptions::new(&url)).unwrap());
    let client = McpClient::new(McpClientOptions::new("http-test", "1.0.0"));
    client.connect(transport).await.unwrap();
    client
        .call_tool("echo", None, McpRequestOptions::default())
        .await
        .unwrap();
    client
        .list_tools(McpRequestOptions::default())
        .await
        .unwrap();
    // The GET stream starts from the initialized notification, asynchronously.
    for _ in 0..100 {
        if order.lock().unwrap().iter().any(|entry| entry == "GET") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    client.close().await.unwrap();
    let order = order.lock().unwrap();
    let initialized = order
        .iter()
        .position(|entry| entry == "notifications/initialized")
        .expect("initialized sent");
    let get = order
        .iter()
        .position(|entry| entry == "GET")
        .expect("GET stream");
    assert!(get > initialized, "{order:?}");
}

#[tokio::test]
async fn reports_a_session_that_expired() {
    let (url, _requests) = start_server(Arc::new(|request: RecordedRequest| -> HandlerResult {
        Box::pin(async move {
            if request.body_method() == Some("tools/list") {
                return TestResponse {
                    status: 404,
                    headers: vec![],
                    body: b"session gone".to_vec(),
                };
            }
            protocol_response(&request)
        })
    }))
    .await;
    let transport =
        Arc::new(StreamableHttpTransport::new(StreamableHttpTransportOptions::new(&url)).unwrap());
    let client = McpClient::new(McpClientOptions::new("http-test", "1.0.0"));
    client.connect(transport).await.unwrap();
    let error = client
        .list_tools(McpRequestOptions::default())
        .await
        .unwrap_err();
    assert!(matches!(error, McpError::SessionExpired), "{error:?}");
}
