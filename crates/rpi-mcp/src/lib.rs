//! Standalone MCP client: port of `@earendil-works/pi-mcp`
//! (`packages/mcp` @ a13d35a74).
//!
//! The crate has no dependency on the rpi host: it carries the two
//! transports (stdio child processes and streamable HTTP), a hand-written
//! JSON-RPC layer, the client session (initialize/tools/resources,
//! notifications, progress, cancellation) and OAuth 2.1 (PKCE, dynamic
//! client registration, loopback callback, cross-process refresh locks stay
//! host-side in `rpi`).

pub mod auth_provider;
pub mod client;
pub mod content;
pub mod oauth;
pub mod protocol;
pub mod transport;
pub mod types;

pub use auth_provider::{AuthProvider, UnauthorizedContext};
pub use client::{
    ClientState, DEFAULT_REQUEST_TIMEOUT_MS, ListenerGuard, McpClient, McpClientOptions,
    McpRequestOptions,
};
pub use content::{LlmContent, block_to_llm_content, to_llm_content};
pub use protocol::{
    JSON_RPC_ERROR_INTERNAL, JSON_RPC_ERROR_INVALID_REQUEST, JSON_RPC_ERROR_METHOD_NOT_FOUND,
    JSON_RPC_ERROR_PARSE, JsonRpcErrorObject, JsonRpcId, JsonRpcMessage, McpError,
    is_json_rpc_notification, is_json_rpc_request, is_json_rpc_response, parse_json_rpc_message,
};
pub use transport::in_memory::{InMemoryTransport, create_in_memory_transport_pair};
pub use transport::stdio::{StderrMode, StdioTransport, StdioTransportOptions};
pub use transport::streamable_http::{
    SseEvent, StreamableHttpReconnectOptions, StreamableHttpTransport,
    StreamableHttpTransportOptions,
};
pub use transport::{
    DEFAULT_MAX_MESSAGE_BYTES, McpTransport, McpTransportError, TransportEvents, Unsubscribe,
};
pub use types::{
    CallToolResult, Implementation, InitializeResult, LATEST_PROTOCOL_VERSION, ListPage,
    ProgressNotification, ReadResourceResult, Resource, ResourceTemplate, Root,
    SUPPORTED_PROTOCOL_VERSIONS, ServerCapabilities, Tool, ToolAnnotations,
};
