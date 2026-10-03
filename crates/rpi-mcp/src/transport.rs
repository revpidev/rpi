//! The transport interface and shared listener bookkeeping (port of
//! `packages/mcp/src/transports/transport.ts` @ a13d35a74).

pub mod in_memory;
pub mod stdio;
pub mod streamable_http;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::protocol::{JsonRpcMessage, McpError};

/// `DEFAULT_MAX_MESSAGE_BYTES` (transport.ts:3).
pub const DEFAULT_MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

/// Transport-level failures (`McpHttpError` family + connection errors).
#[derive(Debug, Clone, thiserror::Error)]
pub enum McpTransportError {
    /// `McpConnectionClosedError`.
    #[error("MCP connection closed")]
    ConnectionClosed,
    /// `McpHttpError` (and subclasses) with the HTTP status.
    #[error("MCP HTTP request failed with status {status}{body_suffix}")]
    Http {
        status: u16,
        message: String,
        body: String,
        body_suffix: String,
    },
    /// `McpAuthRequiredError` (401, or 403 asking for more scope).
    #[error("MCP server requires authentication")]
    AuthRequired {
        www_authenticate: Option<String>,
        body: String,
    },
    /// `McpOAuthAuthorizationRequiredError` (the user has to sign in again).
    #[error("MCP OAuth authorization requires user interaction")]
    AuthorizationRequired,
    /// `McpSessionExpiredError` (404 after a session id was issued).
    #[error("MCP session expired")]
    SessionExpired { body: String },
    /// Local IO failure (spawn, pipe, network).
    #[error("{0}")]
    Io(String),
    /// Any other transport failure.
    #[error("{0}")]
    Other(String),
}

impl McpTransportError {
    pub fn http(status: u16, message: impl Into<String>, body: impl Into<String>) -> Self {
        let body = body.into();
        let message = message.into();
        let body_suffix = if body.is_empty() {
            String::new()
        } else {
            format!(": {message}")
        };
        McpTransportError::Http {
            status,
            message,
            body,
            body_suffix,
        }
    }

    /// HTTP status when this came from a response.
    pub fn status(&self) -> Option<u16> {
        match self {
            McpTransportError::Http { status, .. } => Some(*status),
            McpTransportError::AuthRequired { .. } => Some(401),
            McpTransportError::SessionExpired { .. } => Some(404),
            _ => None,
        }
    }

    /// 401, or 403 with an `insufficient_scope` challenge → sign-in needed.
    pub fn needs_authorization(&self) -> bool {
        matches!(self, McpTransportError::AuthRequired { .. })
    }

    /// Network failures and transient statuses worth another attempt
    /// (runtime.ts:53).
    pub fn is_transient(&self) -> bool {
        match self {
            McpTransportError::Http { status, .. } => {
                *status == 408 || *status == 429 || (*status >= 500 && *status != 501)
            }
            McpTransportError::Io(_) => true,
            _ => false,
        }
    }

    /// The client-facing error for a failed send.
    pub fn into_mcp_error(self) -> McpError {
        match self {
            McpTransportError::ConnectionClosed => McpError::connection_closed(),
            McpTransportError::Http {
                status, message, ..
            } => McpError::HttpError { status, message },
            McpTransportError::AuthRequired { .. } => McpError::AuthRequired,
            McpTransportError::AuthorizationRequired => McpError::AuthorizationRequired,
            McpTransportError::SessionExpired { .. } => McpError::SessionExpired,
            McpTransportError::Io(message) => McpError::Network(message),
            McpTransportError::Other(message) => McpError::Transport(message),
        }
    }
}

/// Listener registration (the three `on*` methods shared by transports).
pub type MessageListener = Arc<dyn Fn(&JsonRpcMessage) + Send + Sync>;
pub type ErrorListener = Arc<dyn Fn(&McpTransportError) + Send + Sync>;
pub type CloseListener = Arc<dyn Fn() + Send + Sync>;

/// RAII handle returned by the `on_*` methods; removing the listener on drop
/// mirrors the unsubscribe closures upstream.
pub struct Unsubscribe {
    inner: Option<Box<dyn FnOnce() + Send + Sync>>,
}

impl Unsubscribe {
    fn new(remove: impl FnOnce() + Send + Sync + 'static) -> Self {
        Self {
            inner: Some(Box::new(remove)),
        }
    }

    /// Run the removal now (upstream's calling the returned closure).
    pub fn unsubscribe(mut self) {
        if let Some(remove) = self.inner.take() {
            remove();
        }
    }
}

impl Drop for Unsubscribe {
    fn drop(&mut self) {
        if let Some(remove) = self.inner.take() {
            remove();
        }
    }
}

struct ListenerSlot<T> {
    next_id: u64,
    listeners: Vec<(u64, T)>,
}

impl<T> Default for ListenerSlot<T> {
    fn default() -> Self {
        Self {
            next_id: 1,
            listeners: Vec::new(),
        }
    }
}

impl<T: Clone> ListenerSlot<T> {
    fn add(&mut self, listener: T) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.listeners.push((id, listener));
        id
    }

    fn remove(&mut self, id: u64) {
        self.listeners.retain(|(existing, _)| *existing != id);
    }

    fn snapshot(&self) -> Vec<T> {
        self.listeners
            .iter()
            .map(|(_, listener)| listener.clone())
            .collect()
    }
}

/// `TransportEvents` (transport.ts:18): listener bookkeeping shared by
/// transports. `emit_close` fires at most once per transport.
#[derive(Default)]
pub struct TransportEvents {
    message: Mutex<ListenerSlot<MessageListener>>,
    error: Mutex<ListenerSlot<ErrorListener>>,
    close: Mutex<ListenerSlot<CloseListener>>,
    close_emitted: Mutex<bool>,
}

impl TransportEvents {
    pub fn on_message(self: &Arc<Self>, listener: MessageListener) -> Unsubscribe {
        let weak = Arc::downgrade(self);
        let id = lock(&self.message).add(listener);
        Unsubscribe::new(move || {
            if let Some(events) = weak.upgrade() {
                lock(&events.message).remove(id);
            }
        })
    }

    pub fn on_error(self: &Arc<Self>, listener: ErrorListener) -> Unsubscribe {
        let weak = Arc::downgrade(self);
        let id = lock(&self.error).add(listener);
        Unsubscribe::new(move || {
            if let Some(events) = weak.upgrade() {
                lock(&events.error).remove(id);
            }
        })
    }

    pub fn on_close(self: &Arc<Self>, listener: CloseListener) -> Unsubscribe {
        let weak = Arc::downgrade(self);
        let id = lock(&self.close).add(listener);
        Unsubscribe::new(move || {
            if let Some(events) = weak.upgrade() {
                lock(&events.close).remove(id);
            }
        })
    }

    pub(crate) fn emit_message(&self, message: &JsonRpcMessage) {
        for listener in lock(&self.message).snapshot() {
            listener(message);
        }
    }

    pub(crate) fn emit_error(&self, error: &McpTransportError) {
        for listener in lock(&self.error).snapshot() {
            listener(error);
        }
    }

    pub(crate) fn emit_close(&self) {
        let mut emitted = lock(&self.close_emitted);
        if *emitted {
            return;
        }
        *emitted = true;
        drop(emitted);
        for listener in lock(&self.close).snapshot() {
            listener();
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

/// A transport the client drives (`McpTransport`, transport.ts:9).
#[async_trait]
pub trait McpTransport: Send + Sync {
    async fn start(&self) -> Result<(), McpTransportError>;
    async fn send(&self, message: serde_json::Value) -> Result<(), McpTransportError>;
    async fn close(&self) -> Result<(), McpTransportError>;
    /// `setProtocolVersion?` (transport.ts:15).
    fn set_protocol_version(&self, _version: &str) {}
    fn events(&self) -> Arc<TransportEvents>;
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn unsubscribe_removes_the_listener() {
        let events = Arc::new(TransportEvents::default());
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        let subscription = events.on_message(Arc::new(move |_| {
            seen.fetch_add(1, Ordering::SeqCst);
        }));
        events.emit_message(&JsonRpcMessage::Notification {
            method: "x".to_owned(),
            params: serde_json::Value::Null,
        });
        assert_eq!(count.load(Ordering::SeqCst), 1);
        subscription.unsubscribe();
        events.emit_message(&JsonRpcMessage::Notification {
            method: "x".to_owned(),
            params: serde_json::Value::Null,
        });
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn close_is_emitted_once() {
        let events = Arc::new(TransportEvents::default());
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        let _subscription = events.on_close(Arc::new(move || {
            seen.fetch_add(1, Ordering::SeqCst);
        }));
        events.emit_close();
        events.emit_close();
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}
