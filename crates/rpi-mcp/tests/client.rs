//! Client session tests, ported from
//! `packages/mcp/test/client.test.ts` @ a13d35a74.

use std::collections::HashMap;
use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rpi_mcp::protocol::{JsonRpcMessage, McpError};
use rpi_mcp::transport::in_memory::{InMemoryTransport, create_in_memory_transport_pair};
use rpi_mcp::{
    LATEST_PROTOCOL_VERSION, McpClient, McpClientOptions, McpRequestOptions, McpTransport,
    McpTransportError,
};
use serde_json::{Value, json};

type Handler = Arc<
    dyn Fn(Value) -> Pin<Box<dyn Future<Output = Result<Value, McpError>> + Send>> + Send + Sync,
>;

fn handler<F>(function: F) -> Handler
where
    F: Fn(Value) -> Pin<Box<dyn Future<Output = Result<Value, McpError>> + Send>>
        + Send
        + Sync
        + 'static,
{
    Arc::new(function)
}

fn ok<F>(function: F) -> Handler
where
    F: Fn(Value) -> Value + Send + Sync + 'static,
{
    handler(move |params| {
        let result = function(params);
        Box::pin(async move { Ok(result) })
    })
}

struct TestServer {
    transport: Arc<InMemoryTransport>,
    messages: Arc<Mutex<Vec<Value>>>,
    handlers: Arc<Mutex<HashMap<String, Handler>>>,
    /// Methods whose responses echo the request id as a JSON float
    /// (`1` -> `1.0`), exercising numeric id matching.
    float_id_methods: Arc<Mutex<HashSet<String>>>,
    _subscription: rpi_mcp::Unsubscribe,
}

impl TestServer {
    fn set_handler(&self, method: &str, value: Handler) {
        self.handlers
            .lock()
            .unwrap()
            .insert(method.to_owned(), value);
    }

    fn messages(&self) -> Vec<Value> {
        self.messages.lock().unwrap().clone()
    }

    fn set_float_id(&self, method: &str) {
        self.float_id_methods
            .lock()
            .unwrap()
            .insert(method.to_owned());
    }
}

async fn create_server() -> (Arc<InMemoryTransport>, TestServer) {
    let (client_transport, server_transport) = create_in_memory_transport_pair();
    let messages: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let handlers: Arc<Mutex<HashMap<String, Handler>>> = Arc::new(Mutex::new(HashMap::new()));
    let float_id_methods: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let subscription = {
        let transport = server_transport.clone();
        let messages = messages.clone();
        let handlers = handlers.clone();
        let float_id_methods = float_id_methods.clone();
        server_transport.events().on_message(Arc::new(move |message: &JsonRpcMessage| {
            messages.lock().unwrap().push(message.to_json());
            let JsonRpcMessage::Request { id, method, params } = message else {
                return;
            };
            let id = id.clone();
            let method = method.clone();
            let params = params.clone();
            let transport = transport.clone();
            let handlers = handlers.clone();
            let float_id_methods = float_id_methods.clone();
            tokio::spawn(async move {
                let handler = handlers.lock().unwrap().get(&method).cloned();
                let response = match handler {
                    Some(handler) => match handler(params).await {
                        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                        Err(error) => match error {
                            McpError::Rpc { code, message, data } => {
                                json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message, "data": data}})
                            }
                            other => json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32603, "message": other.to_string()}}),
                        },
                    },
                    None => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {"code": -32601, "message": format!("Method not found: {method}")},
                    }),
                };
                let mut response = response;
                if float_id_methods.lock().unwrap().contains(&method)
                    && let Some(number) = response.get("id").and_then(Value::as_f64)
                {
                    response["id"] = Value::from(number);
                }
                let _ = transport.send(response).await;
            });
        }))
    };
    server_transport.start().await.unwrap();
    let server = TestServer {
        transport: server_transport,
        messages,
        handlers,
        float_id_methods,
        _subscription: subscription,
    };
    server.set_handler(
        "initialize",
        ok(|_| {
            json!({
                "protocolVersion": LATEST_PROTOCOL_VERSION,
                "capabilities": {"tools": {"listChanged": true}},
                "serverInfo": {"name": "test-server", "version": "1.0.0"},
                "instructions": "Use test tools.",
            })
        }),
    );
    (client_transport, server)
}

async fn connect() -> (Arc<McpClient>, TestServer) {
    let (client_transport, server) = create_server().await;
    let client = McpClient::new(McpClientOptions::new("test-client", "2.0.0"));
    client.connect(client_transport).await.unwrap();
    (client, server)
}

/// v0.1.6 review P3: a server answering with `1.0` must settle the request
/// sent with id `1` (upstream compares JS numbers).
#[tokio::test]
async fn numeric_response_ids_match_by_value() {
    let (client, server) = connect().await;
    server.set_handler("tools/echo", ok(|_| json!({"ok": true})));
    server.set_float_id("tools/echo");
    let result = client
        .request("tools/echo", None, McpRequestOptions::default())
        .await
        .expect("1.0 must match the pending request id 1");
    assert_eq!(result, json!({"ok": true}));
    client.close().await.unwrap();
}

#[tokio::test]
async fn initializes_the_connection_before_exposing_server_information() {
    let (client, server) = connect().await;
    assert_eq!(client.connection_state(), rpi_mcp::ClientState::Connected);
    assert_eq!(
        client.protocol_version().as_deref(),
        Some(LATEST_PROTOCOL_VERSION)
    );
    let info = client.server_info().unwrap();
    assert_eq!(info.name, "test-server");
    assert_eq!(client.instructions().as_deref(), Some("Use test tools."));
    assert_eq!(
        server.messages(),
        vec![
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": LATEST_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": "test-client", "version": "2.0.0"},
                },
            }),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        ]
    );
    client.close().await.unwrap();
}

#[tokio::test]
async fn paginates_tools_and_preserves_definitions() {
    let (client, server) = connect().await;
    server.set_handler(
        "tools/list",
        ok(|params| {
            let cursor = params.get("cursor").and_then(Value::as_str);
            if cursor.is_none() {
                json!({
                    "tools": [{"name": "search", "description": "Search", "inputSchema": {"type": "object"}}],
                    "nextCursor": "page-2",
                })
            } else {
                json!({
                    "tools": [{
                        "name": "read",
                        "inputSchema": {"type": "object"},
                        "outputSchema": {"type": "object"},
                        "annotations": {"readOnlyHint": true},
                    }],
                    "nextCursor": "",
                })
            }
        }),
    );
    let tools = client
        .list_tools(McpRequestOptions::default())
        .await
        .unwrap();
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0].name, "search");
    assert_eq!(tools[0].description.as_deref(), Some("Search"));
    assert_eq!(tools[1].name, "read");
    assert_eq!(
        tools[1].annotations.as_ref().unwrap().read_only_hint,
        Some(true)
    );
    client.close().await.unwrap();
}

#[tokio::test]
async fn lists_and_reads_resources() {
    let (client, server) = connect().await;
    server.set_handler(
        "resources/list",
        ok(|params| {
            if params.get("cursor").is_none() {
                json!({"resources": [{"uri": "file:///a", "name": "a", "mimeType": "text/plain"}], "nextCursor": "2"})
            } else {
                json!({"resources": [{"uri": "file:///b"}]})
            }
        }),
    );
    server.set_handler(
        "resources/templates/list",
        ok(|_| json!({"resourceTemplates": [{"uriTemplate": "repo://{owner}/{repo}", "name": "repo"}]})),
    );
    server.set_handler(
        "resources/read",
        ok(|params| json!({"contents": [{"uri": params.get("uri").cloned().unwrap_or(Value::Null), "text": "hello"}]})),
    );
    let resources = client
        .list_resources(McpRequestOptions::default())
        .await
        .unwrap();
    assert_eq!(resources.len(), 2);
    assert_eq!(resources[1].name, "file:///b");
    let templates = client
        .list_resource_templates(McpRequestOptions::default())
        .await
        .unwrap();
    assert_eq!(templates[0].uri_template, "repo://{owner}/{repo}");
    let (page, next) = client
        .list_resources_page(None, McpRequestOptions::default())
        .await
        .unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(next.as_deref(), Some("2"));
    let (page, next) = client
        .list_resources_page(Some("2".to_owned()), McpRequestOptions::default())
        .await
        .unwrap();
    assert_eq!(page.len(), 1);
    assert!(next.is_none());
    let read = client
        .read_resource("file:///a", McpRequestOptions::default())
        .await
        .unwrap();
    assert_eq!(read["contents"][0]["text"], "hello");

    server.set_handler(
        "resources/read",
        ok(|_| json!({"contents": [{"uri": "file:///a"}]})),
    );
    assert!(
        client
            .read_resource("file:///a", McpRequestOptions::default())
            .await
            .is_err()
    );
    server.set_handler(
        "resources/list",
        ok(|_| json!({"resources": [{"name": "no uri"}]})),
    );
    assert!(
        client
            .list_resources(McpRequestOptions::default())
            .await
            .is_err()
    );
    client.close().await.unwrap();
}

#[tokio::test]
async fn returns_structured_tool_content_and_surfaces_json_rpc_errors() {
    let (client, server) = connect().await;
    server.set_handler(
        "tools/call",
        handler(|params| {
            Box::pin(async move {
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                if name == "fail" {
                    return Err(McpError::Rpc {
                        code: 1234,
                        message: "tool failed".to_owned(),
                        data: Some(json!({"retryable": false})),
                    });
                }
                let count = params
                    .get("arguments")
                    .and_then(|args| args.get("count"))
                    .cloned()
                    .unwrap_or(Value::Null);
                Ok(json!({
                    "content": [{"type": "text", "text": "ok"}],
                    "structuredContent": {"count": count},
                }))
            })
        }),
    );
    let result = client
        .call_tool(
            "count",
            Some(json!({"count": 3})),
            McpRequestOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(result.content[0]["text"], "ok");
    assert_eq!(result.structured_content, Some(json!({"count": 3})));
    let error = client
        .call_tool("fail", None, McpRequestOptions::default())
        .await
        .unwrap_err();
    assert_eq!(error.rpc_code(), Some(1234));
    assert_eq!(error.rpc_data(), Some(&json!({"retryable": false})));
    client.close().await.unwrap();
}

#[tokio::test]
async fn renews_the_timeout_on_progress() {
    let (client, server) = connect().await;
    let progress_calls = Arc::new(Mutex::new(Vec::new()));
    server.set_handler(
        "tools/call",
        handler({
            let server = server.transport.clone();
            move |params| {
                let server = server.clone();
                Box::pin(async move {
                    let token = params
                        .get("_meta")
                        .and_then(|meta| meta.get("progressToken"))
                        .cloned();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(150)).await;
                        let _ = server
                            .send(json!({
                                "jsonrpc": "2.0",
                                "method": "notifications/progress",
                                "params": {"progressToken": token, "progress": 1, "total": 2},
                            }))
                            .await;
                    });
                    tokio::time::sleep(Duration::from_millis(400)).await;
                    Ok(json!({"content": [{"type": "text", "text": "done"}]}))
                })
            }
        }),
    );
    let progress = progress_calls.clone();
    let result = client.call_tool(
        "slow",
        None,
        McpRequestOptions {
            // Rearmed by the progress notification at t=150ms; the handler
            // finishes at t=400ms, so the call succeeds only when rearmed.
            timeout_ms: Some(300),
            on_progress: Some(Arc::new(move |notification| {
                progress.lock().unwrap().push(notification.progress);
            })),
            ..Default::default()
        },
    );
    let result = tokio::time::timeout(Duration::from_secs(5), result)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.content[0]["text"], "done");
    assert_eq!(progress_calls.lock().unwrap().as_slice(), &[1.0]);
    client.close().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn cancels_aborted_and_timed_out_requests() {
    let (client, server) = connect().await;
    server.set_handler("tools/call", handler(|_| Box::pin(std::future::pending())));
    let signal = tokio_util::sync::CancellationToken::new();
    let aborted = {
        let client = client.clone();
        let signal = signal.clone();
        tokio::spawn(async move {
            client
                .call_tool(
                    "wait",
                    None,
                    McpRequestOptions {
                        signal: Some(signal),
                        ..Default::default()
                    },
                )
                .await
        })
    };
    // Let the request register and send before aborting (the future is lazy
    // in Rust, unlike the upstream call).
    for _ in 0..100 {
        if server
            .messages()
            .iter()
            .any(|message| message.get("method").and_then(Value::as_str) == Some("tools/call"))
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    signal.cancel();
    let error = aborted.await.unwrap().unwrap_err();
    assert!(matches!(error, McpError::Aborted), "{error:?}");
    let mut cancelled = false;
    for _ in 0..100 {
        if server.messages().iter().any(|message| {
            message.get("method").and_then(Value::as_str) == Some("notifications/cancelled")
                && message["params"]["requestId"] == 2
        }) {
            cancelled = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert!(cancelled, "{:?}", server.messages());

    let error = client
        .call_tool(
            "wait",
            None,
            McpRequestOptions {
                timeout_ms: Some(5),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(error, McpError::Timeout { .. }), "{error:?}");
    client.close().await.unwrap();
}

#[tokio::test]
async fn reports_transport_errors_without_failing_pending_requests() {
    let (client_transport, server) = create_server().await;
    let client = McpClient::new(McpClientOptions::new("test-client", "1.0.0"));
    client.connect(client_transport.clone()).await.unwrap();
    let errors: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let errors = errors.clone();
        client.on_error(Arc::new(move |error| {
            errors.lock().unwrap().push(error.to_string());
        }));
    }
    server.set_handler(
        "tools/call",
        handler(|_| {
            Box::pin(async {
                tokio::time::sleep(Duration::from_millis(10)).await;
                Ok(json!({"content": []}))
            })
        }),
    );
    let call = client.call_tool("wait", None, McpRequestOptions::default());
    tokio::task::yield_now().await;
    client_transport.emit_error_for_test(McpTransportError::Other("stray log line".to_owned()));
    let result = call.await.unwrap();
    assert!(result.content.is_empty());
    assert_eq!(
        errors.lock().unwrap().as_slice(),
        &["stray log line".to_owned()]
    );
    client.close().await.unwrap();
}

#[tokio::test]
async fn accepts_servers_that_answer_with_an_older_protocol_version() {
    let (client_transport, server) = create_server().await;
    server.set_handler(
        "initialize",
        ok(|_| {
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "serverInfo": {"name": "old-server", "version": "0.1.0"},
            })
        }),
    );
    let client = McpClient::new(McpClientOptions::new("test-client", "1.0.0"));
    client.connect(client_transport).await.unwrap();
    assert_eq!(client.protocol_version().as_deref(), Some("2024-11-05"));
    client.close().await.unwrap();

    let (client_transport, server) = create_server().await;
    server.set_handler(
        "initialize",
        ok(|_| {
            json!({
                "protocolVersion": "1999-01-01",
                "capabilities": {},
                "serverInfo": {"name": "ancient-server", "version": "0.1.0"},
            })
        }),
    );
    let client = McpClient::new(McpClientOptions::new("test-client", "1.0.0"));
    let error = client.connect(client_transport).await.unwrap_err();
    assert!(
        error.to_string().contains("unsupported protocol version"),
        "{error}"
    );
    assert_eq!(client.connection_state(), rpi_mcp::ClientState::Closed);
}

#[tokio::test]
async fn defaults_missing_tool_result_content_to_an_empty_list() {
    let (client, server) = connect().await;
    server.set_handler(
        "tools/call",
        ok(|_| json!({"structuredContent": {"ok": true}})),
    );
    let result = client
        .call_tool("structured", None, McpRequestOptions::default())
        .await
        .unwrap();
    assert!(result.content.is_empty());
    server.set_handler("tools/call", ok(|_| json!({"content": "not a list"})));
    assert!(
        client
            .call_tool("broken", None, McpRequestOptions::default())
            .await
            .is_err()
    );
    client.close().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn does_not_send_notifications_cancelled_for_a_timed_out_initialize() {
    let (client_transport, server) = create_server().await;
    server.set_handler("initialize", handler(|_| Box::pin(std::future::pending())));
    let client = McpClient::new(McpClientOptions {
        request_timeout_ms: Some(5),
        ..McpClientOptions::new("test-client", "1.0.0")
    });
    let error = client.connect(client_transport).await.unwrap_err();
    assert!(matches!(error, McpError::Timeout { .. }), "{error:?}");
    tokio::task::yield_now().await;
    assert!(
        !server
            .messages()
            .iter()
            .any(|message| message.get("method").and_then(Value::as_str)
                == Some("notifications/cancelled"))
    );
}

#[tokio::test]
async fn notifies_close_listeners_once_when_the_transport_drops() {
    let (client, server) = connect().await;
    let closed = Arc::new(Mutex::new(0usize));
    {
        let closed = closed.clone();
        client.on_close(Arc::new(move || {
            *closed.lock().unwrap() += 1;
        }));
    }
    server.set_handler("tools/call", handler(|_| Box::pin(std::future::pending())));
    let pending = client.call_tool("wait", None, McpRequestOptions::default());
    tokio::task::yield_now().await;
    server.transport.close().await.unwrap();
    let error = pending.await.unwrap_err();
    assert!(
        error.to_string().contains("MCP connection closed"),
        "{error}"
    );
    assert_eq!(client.connection_state(), rpi_mcp::ClientState::Closed);
    client.close().await.unwrap();
    assert_eq!(*closed.lock().unwrap(), 1);
}

#[tokio::test]
async fn answers_roots_list_and_dispatches_notifications() {
    let (client_transport, server) = create_server().await;
    let client = McpClient::new(McpClientOptions {
        roots: vec![rpi_mcp::Root {
            uri: "file:///workspace".to_owned(),
            name: Some("workspace".to_owned()),
        }],
        ..McpClientOptions::new("test-client", "1.0.0")
    });
    client.connect(client_transport).await.unwrap();
    let changed = Arc::new(Mutex::new(0usize));
    {
        let changed = changed.clone();
        client.on_notification(
            "notifications/tools/list_changed",
            Arc::new(move |_| {
                *changed.lock().unwrap() += 1;
            }),
        );
    }
    server
        .transport
        .send(json!({"jsonrpc": "2.0", "id": "roots", "method": "roots/list"}))
        .await
        .unwrap();
    server
        .transport
        .send(json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"}))
        .await
        .unwrap();
    let mut responded = false;
    for _ in 0..100 {
        responded = server.messages().iter().any(|message| {
            message.get("id").and_then(Value::as_str) == Some("roots")
                && message["result"]["roots"][0]["uri"] == "file:///workspace"
        });
        if responded && *changed.lock().unwrap() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert!(responded, "{:?}", server.messages());
    assert_eq!(*changed.lock().unwrap(), 1);
    client.close().await.unwrap();
}
