//! In-memory transport pair for tests (port of
//! `packages/mcp/src/transports/in-memory.ts` @ a13d35a74).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use super::{McpTransport, McpTransportError, TransportEvents};

/// `InMemoryTransport` (in-memory.ts:4).
pub struct InMemoryTransport {
    events: Arc<TransportEvents>,
    peer: Mutex<Option<Arc<InMemoryTransport>>>,
    started: AtomicBool,
    closed: AtomicBool,
}

impl Default for InMemoryTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryTransport {
    pub fn new() -> Self {
        Self {
            events: Arc::new(TransportEvents::default()),
            peer: Mutex::new(None),
            started: AtomicBool::new(false),
            closed: AtomicBool::new(false),
        }
    }

    /// `connectPeer` (in-memory.ts:10).
    pub fn connect_peer(self: &Arc<Self>, peer: Arc<Self>) -> Result<(), String> {
        let mut slot = lock(&self.peer);
        if slot.is_some() {
            return Err("In-memory MCP transport already has a peer".to_owned());
        }
        *slot = Some(peer);
        Ok(())
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// `emitError` exposed for tests (in-memory.ts:37).
    pub fn emit_error_for_test(&self, error: McpTransportError) {
        self.events.emit_error(&error);
    }

    pub fn emit_message_for_test(&self, message: &crate::protocol::JsonRpcMessage) {
        self.events.emit_message(message);
    }

    fn deliver(&self, message: serde_json::Value) {
        if self.closed.load(Ordering::SeqCst) {
            return;
        }
        if let Some(parsed) = crate::protocol::parse_json_rpc_message(message) {
            self.events.emit_message(&parsed);
        }
    }
}

#[async_trait]
impl McpTransport for InMemoryTransport {
    async fn start(&self) -> Result<(), McpTransportError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(McpTransportError::ConnectionClosed);
        }
        self.started.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn send(&self, message: serde_json::Value) -> Result<(), McpTransportError> {
        if !self.started.load(Ordering::SeqCst) || self.closed.load(Ordering::SeqCst) {
            return Err(McpTransportError::ConnectionClosed);
        }
        let peer = lock(&self.peer).clone();
        let Some(peer) = peer else {
            return Err(McpTransportError::Other(
                "In-memory MCP peer is not connected".to_owned(),
            ));
        };
        if !peer.started.load(Ordering::SeqCst) || peer.closed.load(Ordering::SeqCst) {
            return Err(McpTransportError::Other(
                "In-memory MCP peer is not connected".to_owned(),
            ));
        }
        // Delivery is synchronous: the upstream `queueMicrotask` still runs
        // before the sender's next await continues, and tests rely on the
        // server having seen a message once `send` returns.
        peer.deliver(message);
        Ok(())
    }

    async fn close(&self) -> Result<(), McpTransportError> {
        if self.closed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        self.events.emit_close();
        let peer = lock(&self.peer).clone();
        if let Some(peer) = peer {
            peer.close().await?;
        }
        Ok(())
    }

    fn events(&self) -> Arc<TransportEvents> {
        self.events.clone()
    }
}

/// `createInMemoryTransportPair` (in-memory.ts:44).
pub fn create_in_memory_transport_pair() -> (Arc<InMemoryTransport>, Arc<InMemoryTransport>) {
    let client = Arc::new(InMemoryTransport::new());
    let server = Arc::new(InMemoryTransport::new());
    client.connect_peer(server.clone()).expect("fresh pair");
    server.connect_peer(client.clone()).expect("fresh pair");
    (client, server)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}
