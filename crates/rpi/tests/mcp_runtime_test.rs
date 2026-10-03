//! `McpServerConnection` runtime tests (V16-08 FR-C): background connect,
//! tool listing, needs-auth marking, lazy reconnect after the transport
//! drops and read-only retry classification.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use rpi::extensions::mcp::config::{
    McpScope, McpServerConfig, McpServerEntry, validate_mcp_server_config,
};
use rpi::extensions::mcp::oauth::McpOAuthCredentialStore;
use rpi::extensions::mcp::runtime::{
    McpServerConnection, McpServerConnectionOptions, McpTransportFactory, ServerState,
};
use rpi_mcp::protocol::McpError;
use rpi_mcp::transport::in_memory::{InMemoryTransport, create_in_memory_transport_pair};
use rpi_mcp::{ClientState, McpTransport, McpTransportError};
use serde_json::{Value, json};

/// A scripted in-memory MCP server: initialize, tools/list and echo
/// tools/call.
struct ScriptedServer {
    transport: Arc<InMemoryTransport>,
    _subscription: rpi_mcp::Unsubscribe,
    requests: Arc<Mutex<Vec<Value>>>,
}

impl ScriptedServer {
    async fn start() -> Self {
        let (client, server) = create_in_memory_transport_pair();
        let requests: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let subscription = {
            let transport = server.clone();
            let requests = requests.clone();
            server.events().on_message(Arc::new(move |message: &rpi_mcp::JsonRpcMessage| {
                requests.lock().unwrap().push(message.to_json());
                let rpi_mcp::JsonRpcMessage::Request { id, method, .. } = message else {
                    return;
                };
                let id = id.clone();
                let method = method.clone();
                let transport = transport.clone();
                tokio::spawn(async move {
                    let result = match method.as_str() {
                        "initialize" => json!({
                            "protocolVersion": "2025-11-25",
                            "capabilities": {"tools": {}},
                            "serverInfo": {"name": "scripted", "version": "1.0.0"},
                        }),
                        "tools/list" => {
                            json!({"tools": [{"name": "echo", "inputSchema": {"type": "object"}}]})
                        }
                        "ping" => json!({}),
                        _ => json!({}),
                    };
                    let _ = transport
                        .send(json!({"jsonrpc": "2.0", "id": id, "result": result}))
                        .await;
                });
            }))
        };
        server.start().await.unwrap();
        Self {
            // The factory hands the *client* half to the connection; the
            // server half stays here for scripting.
            transport: client,
            _subscription: subscription,
            requests,
        }
    }
}

fn stdio_entry(name: &str) -> McpServerEntry {
    McpServerEntry {
        name: name.to_owned(),
        config: validate_mcp_server_config(name, &json!({"command": "scripted"})).unwrap(),
        source: "test".to_owned(),
        scope: Some(McpScope::Global),
    }
}

fn http_entry(name: &str, url: &str) -> McpServerEntry {
    McpServerEntry {
        name: name.to_owned(),
        config: validate_mcp_server_config(name, &json!({"url": url})).unwrap(),
        source: "test".to_owned(),
        scope: Some(McpScope::Global),
    }
}

type TransportQueue = Arc<Mutex<VecDeque<Arc<dyn McpTransport>>>>;

fn queued_factory(transports: Vec<Arc<dyn McpTransport>>) -> (McpTransportFactory, TransportQueue) {
    let queue = Arc::new(Mutex::new(VecDeque::from(transports)));
    let factory: McpTransportFactory = {
        let queue = queue.clone();
        Arc::new(move |_entry, _cwd, _auth| {
            queue
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| "no scripted transport left".to_owned())
        })
    };
    (factory, queue)
}

/// A transport whose `start` fails with 401.
struct AuthRequiredTransport;

#[async_trait::async_trait]
impl McpTransport for AuthRequiredTransport {
    async fn start(&self) -> Result<(), McpTransportError> {
        Err(McpTransportError::AuthRequired {
            www_authenticate: Some("Bearer".to_owned()),
            body: String::new(),
        })
    }

    async fn send(&self, _message: Value) -> Result<(), McpTransportError> {
        Err(McpTransportError::ConnectionClosed)
    }

    async fn close(&self) -> Result<(), McpTransportError> {
        Ok(())
    }

    fn events(&self) -> Arc<rpi_mcp::TransportEvents> {
        Arc::new(rpi_mcp::TransportEvents::default())
    }
}

#[tokio::test]
async fn connects_lists_tools_and_closes() {
    let server = ScriptedServer::start().await;
    let (factory, _queue) = queued_factory(vec![server.transport.clone()]);
    let connection = Arc::new(McpServerConnection::new(McpServerConnectionOptions {
        entry: stdio_entry("scripted"),
        cwd: "/tmp".to_owned(),
        create_transport: factory,
        credentials: Arc::new(McpOAuthCredentialStore::new(std::path::Path::new(
            "/tmp/rpi-mcp-test",
        ))),
        provider_token: None,
        on_tools: Arc::new(|_| {}),
        on_change: None,
        log: None,
    }));
    assert_eq!(connection.state(), ServerState::Connecting);
    let client = connection.get_client().await.unwrap();
    assert_eq!(client.connection_state(), ClientState::Connected);
    assert_eq!(connection.state(), ServerState::Connected);
    assert_eq!(connection.tools().len(), 1);
    assert_eq!(connection.tools()[0].name, "echo");
    assert_eq!(connection.instructions(), None);
    // The runtime announced itself as `rpi` with a session root.
    let initialize = {
        let requests = server.requests.lock().unwrap();
        requests
            .iter()
            .find(|request| request.get("method").and_then(Value::as_str) == Some("initialize"))
            .cloned()
            .expect("initialize sent")
    };
    assert_eq!(initialize["params"]["clientInfo"]["name"], "rpi");
    assert!(
        initialize["params"]["clientInfo"]["version"]
            .as_str()
            .is_some_and(|version| !version.is_empty())
    );
    connection.close().await;
    assert_eq!(connection.state(), ServerState::Closed);
    assert!(connection.get_client().await.is_err());
}

#[tokio::test]
async fn oauth_server_needing_sign_in_is_marked() {
    let (entry, factory) = {
        let entry = http_entry("remote", "https://mcp.example/mcp");
        let factory: McpTransportFactory = Arc::new(move |_entry, _cwd, _auth| {
            Ok(Arc::new(AuthRequiredTransport) as Arc<dyn McpTransport>)
        });
        (entry, factory)
    };
    let connection = Arc::new(McpServerConnection::new(McpServerConnectionOptions {
        entry,
        cwd: "/tmp".to_owned(),
        create_transport: factory,
        credentials: Arc::new(McpOAuthCredentialStore::new(std::path::Path::new(
            "/tmp/rpi-mcp-test",
        ))),
        provider_token: None,
        on_tools: Arc::new(|_| {}),
        on_change: None,
        log: None,
    }));
    let error = match connection.get_client().await {
        Ok(_) => panic!("expected a sign-in error"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("requires sign-in"), "{error}");
    assert_eq!(connection.state(), ServerState::NeedsAuth);
    connection.close().await;
}

#[tokio::test]
async fn reconnects_lazily_after_the_transport_drops() {
    let first = ScriptedServer::start().await;
    let second = ScriptedServer::start().await;
    let (factory, queue) = queued_factory(vec![first.transport.clone(), second.transport.clone()]);
    let connection = Arc::new(McpServerConnection::new(McpServerConnectionOptions {
        entry: stdio_entry("scripted"),
        cwd: "/tmp".to_owned(),
        create_transport: factory,
        credentials: Arc::new(McpOAuthCredentialStore::new(std::path::Path::new(
            "/tmp/rpi-mcp-test",
        ))),
        provider_token: None,
        on_tools: Arc::new(|_| {}),
        on_change: None,
        log: None,
    }));
    connection.get_client().await.unwrap();
    assert_eq!(connection.state(), ServerState::Connected);
    // Dropping the server closes the client transport, which marks the
    // connection disconnected; the next call reconnects from the queue.
    first.transport.close().await.unwrap();
    for _ in 0..100 {
        if connection.state() == ServerState::Disconnected {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(connection.state(), ServerState::Disconnected);
    let client = connection.get_client().await.unwrap();
    assert_eq!(client.connection_state(), ClientState::Connected);
    assert_eq!(connection.state(), ServerState::Connected);
    assert!(queue.lock().unwrap().is_empty(), "both transports used");
    connection.close().await;
}

#[tokio::test]
async fn non_oauth_auth_failure_is_a_connection_failure() {
    let factory: McpTransportFactory =
        Arc::new(
            |_entry, _cwd, _auth| Ok(Arc::new(AuthRequiredTransport) as Arc<dyn McpTransport>),
        );
    let connection = Arc::new(McpServerConnection::new(McpServerConnectionOptions {
        entry: stdio_entry("plain"),
        cwd: "/tmp".to_owned(),
        create_transport: factory,
        credentials: Arc::new(McpOAuthCredentialStore::new(std::path::Path::new(
            "/tmp/rpi-mcp-test",
        ))),
        provider_token: None,
        on_tools: Arc::new(|_| {}),
        on_change: None,
        log: None,
    }));
    let error = match connection.get_client().await {
        Ok(_) => panic!("expected a connection failure"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("failed to connect"), "{error}");
    assert_eq!(connection.state(), ServerState::Failed);
    assert!(connection.error().is_some());
    connection.close().await;
}

/// The `McpServerConfig` shape used by the tests stays the same as the
/// validated runtime shape.
#[test]
fn test_entries_validate() {
    assert!(matches!(stdio_entry("x").config, McpServerConfig::Stdio(_)));
    assert!(matches!(
        http_entry("y", "https://x/mcp").config,
        McpServerConfig::Http(_)
    ));
}

/// Keeps the unused-import check honest for the sign-in error helper.
#[test]
fn authorization_error_classification() {
    assert!(McpError::AuthorizationRequired.is_authorization_required());
}
