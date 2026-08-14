//! Streamable HTTP session recovery: Mcp-Session-Id expiry → reconnect
//! → retry once (FR-P1-08).
//!
//! Port of `session-recovery.ts` @ 3d953f90.
//!
//! Wired into `proxy.rs::execute_call` and `direct.rs::execute_direct_tool`
//! (TE-D11): `is_terminated_session` gates the retry; the executors perform
//! the identity-guarded reconnect and replay the call exactly once (the
//! `withSessionRecovery` wrapper is kept for library consumers).
//!
//! Per the MCP spec: a 404 for a request that carried `Mcp-Session-Id`
//! means the session is stale (server restarted, lost session table).
//! The retry against a freshly initialized session cannot double-execute
//! because the server rejects before processing.

use std::sync::Arc;
use std::time::Duration;

use crate::manager::{ConnectionStatus, McpServerManager, ServerConnection};
use crate::metadata::McpConfig;
use crate::protocol::ProtocolError;

/// `-32000` (CONNECTION_CLOSED_PROTOCOL_CODE, session-recovery.ts:41): the
/// JSON-RPC error code for the "server not initialized" gate some servers
/// emit before dispatching to a handler.
const CONNECTION_CLOSED_PROTOCOL_CODE: i64 = -32000;

/// `SERVER_NOT_INITIALIZED_MCP_MESSAGES` (session-recovery.ts:42-45).
const SERVER_NOT_INITIALIZED_MESSAGES: &[&str] = &[
    "Server not initialized",
    "Bad Request: Server not initialized",
];

/// `isTerminatedSession` (session-recovery.ts:47-58): true only when the
/// error indicates a stale session AND the connection had a session id.
pub fn is_terminated_session(error: &ProtocolError, had_session_id: bool) -> bool {
    if !had_session_id {
        return false;
    }
    match error {
        // 404 is the spec's own stale-session signal; a 400 whose body is
        // the -32000 "Bad Request: Server not initialized" JSON-RPC error
        // is the same signal in SdkHttpError clothing (upstream regexes the
        // body; we scan by hand to stay regex-free here).
        ProtocolError::Http { status: 404, .. } => true,
        ProtocolError::Http {
            status: 400,
            message,
        } => {
            // Equivalent of /"code"\s*:\s*-32000/ and /"message"\s*:\s*
            // "Bad Request: Server not initialized"/ — whitespace is
            // allowed on both sides of the colon.
            has_json_field(message, "code", "-32000")
                && has_json_field(
                    message,
                    "message",
                    "\"Bad Request: Server not initialized\"",
                )
        }
        // ProtocolError (SDK): code -32000 with one of the exact
        // server-not-initialized messages.
        ProtocolError::Rpc { code, message, .. } => {
            *code == CONNECTION_CLOSED_PROTOCOL_CODE
                && SERVER_NOT_INITIALIZED_MESSAGES.contains(&message.as_str())
        }
        // Some servers emit -32000 "Server not initialized" before dispatch.
        ProtocolError::Protocol(msg) => {
            // Check for the JSON-RPC error shape embedded in the message.
            let has_code = msg.contains("-32000");
            let has_msg = SERVER_NOT_INITIALIZED_MESSAGES
                .iter()
                .any(|s| msg.contains(s));
            has_code && has_msg
        }
        _ => false,
    }
}

/// Hand-rolled equivalent of the upstream regex fragment
/// `"<field>"\s*:\s*<value>` (session-recovery.ts:52-54): the quoted field
/// name followed by a colon — with optional ASCII whitespace on either
/// side — and then the literal value. Keeps session_recovery regex-free.
fn has_json_field(message: &str, field: &str, value: &str) -> bool {
    let quoted_field = format!("\"{field}\"");
    let mut search_from = 0;
    while let Some(found) = message[search_from..].find(&quoted_field) {
        let after_field = search_from + found + quoted_field.len();
        let rest = message[after_field..].trim_start_matches(|c: char| c.is_ascii_whitespace());
        if let Some(stripped) = rest.strip_prefix(':') {
            let rest = stripped.trim_start_matches(|c: char| c.is_ascii_whitespace());
            if rest.starts_with(value) {
                return true;
            }
        }
        search_from = after_field;
    }
    false
}

/// Check if a connection has a session id (only Streamable HTTP does).
fn has_session_id(connection: &ServerConnection) -> bool {
    // The client stores the session id; we check via the client's transport.
    // For stdio/SSE transports this is always None.
    connection
        .client
        .as_ref()
        .map(|c| c.session_id().is_some())
        .unwrap_or(false)
}

/// `SessionRecoveryAuthRequiredError` (session-recovery.ts:68-73).
pub fn auth_required_error(server_name: &str) -> ProtocolError {
    ProtocolError::Protocol(auth_required_error_message(server_name))
}

/// The `SessionRecoveryAuthRequiredError` message (session-recovery.ts:71).
pub fn auth_required_error_message(server_name: &str) -> String {
    format!("MCP server \"{server_name}\" requires OAuth authentication after reconnect.")
}

/// Callback type for `on_needs_auth` (session recovery auth retry).
type NeedsAuthCallback<'a> =
    &'a dyn Fn(&str) -> futures::future::BoxFuture<'static, Option<Arc<ServerConnection>>>;

/// `SessionRecoveryDeps` (session-recovery.ts:75-80).
pub struct SessionRecoveryDeps<'a> {
    pub manager: &'a Arc<McpServerManager>,
    pub config: &'a McpConfig,
    pub request_timeout: Duration,
    /// Called when the fresh connection is `needs-auth`; should attempt
    /// auto-auth and return the updated connection (or None).
    pub on_needs_auth: Option<NeedsAuthCallback<'a>>,
}

/// `withSessionRecovery` (session-recovery.ts:93-151): run `fn` against the
/// current connection; on a terminated-session error, reconnect exactly
/// once (identity-guarded via `McpServerManager::reconnect`) and retry.
pub async fn with_session_recovery<T, F>(
    deps: &SessionRecoveryDeps<'_>,
    server_name: &str,
    fn_impl: F,
) -> Result<T, ProtocolError>
where
    F: Fn(Arc<ServerConnection>) -> futures::future::BoxFuture<'static, Result<T, ProtocolError>>,
{
    let definition = deps.config.mcp_servers.get(server_name);
    let definition = definition.ok_or_else(|| {
        ProtocolError::Protocol(format!("Server \"{server_name}\" is not in config"))
    })?;

    if definition.is_disabled() {
        return Err(ProtocolError::Protocol(format!(
            "MCP server \"{server_name}\" is disabled"
        )));
    }

    let connection = deps.manager.get_connection(server_name).ok_or_else(|| {
        ProtocolError::Protocol(format!("Server \"{server_name}\" is not connected"))
    })?;

    let had_session_id = has_session_id(&connection);

    match fn_impl(connection.clone()).await {
        Ok(result) => Ok(result),
        Err(error) => {
            if !is_terminated_session(&error, had_session_id) {
                return Err(error);
            }

            // Reconnect (identity-guarded by Arc ptr equality inside manager).
            let fresh = deps
                .manager
                .reconnect(server_name, definition, &connection)
                .await?;

            if fresh.status() == ConnectionStatus::NeedsAuth {
                if let Some(on_needs_auth) = deps.on_needs_auth {
                    let _ = on_needs_auth(server_name).await;
                }
                let fresh2 = deps.manager.get_connection(server_name);
                match fresh2 {
                    Some(c) if c.status() == ConnectionStatus::Connected => {
                        return fn_impl(c).await;
                    }
                    _ => return Err(auth_required_error(server_name)),
                }
            }

            if fresh.status() != ConnectionStatus::Connected {
                return Err(error);
            }

            fn_impl(fresh).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_terminated_session_404_with_session_id() {
        let error = ProtocolError::Http {
            status: 404,
            message: "not found".to_string(),
        };
        assert!(is_terminated_session(&error, true));
    }

    #[test]
    fn is_terminated_session_404_without_session_id() {
        let error = ProtocolError::Http {
            status: 404,
            message: "not found".to_string(),
        };
        assert!(!is_terminated_session(&error, false));
    }

    #[test]
    fn is_terminated_session_server_not_initialized() {
        let error = ProtocolError::Protocol(
            r#"{"code":-32000,"message":"Bad Request: Server not initialized"}"#.to_string(),
        );
        assert!(is_terminated_session(&error, true));
    }

    #[test]
    fn is_terminated_session_generic_error_not_matched() {
        let error = ProtocolError::Protocol("some other error".to_string());
        assert!(!is_terminated_session(&error, true));
    }

    #[test]
    fn is_terminated_session_500_not_matched() {
        let error = ProtocolError::Http {
            status: 500,
            message: "internal error".to_string(),
        };
        assert!(!is_terminated_session(&error, true));
    }

    #[test]
    fn is_terminated_session_400_with_body_code_and_message() {
        // session-recovery.ts:52-54: SdkHttpError 400 whose body matches
        // both the -32000 code and the exact "Bad Request: Server not
        // initialized" message.
        let error = ProtocolError::Http {
            status: 400,
            message: r#"Error POSTing to endpoint: {"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"Bad Request: Server not initialized"}}"#.to_string(),
        };
        assert!(is_terminated_session(&error, true));
    }

    #[test]
    fn is_terminated_session_400_with_whitespace_in_body() {
        // The upstream regex allows whitespace after the colons
        // (/\"code\"\s*:\s*-32000/).
        let error = ProtocolError::Http {
            status: 400,
            message: r#"{"code":  -32000, "message":  "Bad Request: Server not initialized"}"#
                .to_string(),
        };
        assert!(is_terminated_session(&error, true));
    }

    #[test]
    fn is_terminated_session_400_other_body_not_matched() {
        let error = ProtocolError::Http {
            status: 400,
            message: r#"{"code":-32602,"message":"Invalid params"}"#.to_string(),
        };
        assert!(!is_terminated_session(&error, true));
    }

    #[test]
    fn is_terminated_session_rpc_code_with_exact_message() {
        // ProtocolError (SDK): code === -32000 and message in the set.
        for message in [
            "Server not initialized",
            "Bad Request: Server not initialized",
        ] {
            let error = ProtocolError::Rpc {
                code: -32000,
                message: message.to_string(),
                data: None,
            };
            assert!(is_terminated_session(&error, true), "message: {message}");
        }
    }

    #[test]
    fn is_terminated_session_rpc_other_code_or_message_not_matched() {
        let wrong_code = ProtocolError::Rpc {
            code: -32001,
            message: "Server not initialized".to_string(),
            data: None,
        };
        assert!(!is_terminated_session(&wrong_code, true));
        let wrong_message = ProtocolError::Rpc {
            code: -32000,
            message: "Something else".to_string(),
            data: None,
        };
        assert!(!is_terminated_session(&wrong_message, true));
        // Substring containment is NOT enough for the Rpc variant — the
        // upstream Set has exact membership.
        let padded = ProtocolError::Rpc {
            code: -32000,
            message: "xServer not initializedx".to_string(),
            data: None,
        };
        assert!(!is_terminated_session(&padded, true));
    }

    #[test]
    fn is_terminated_session_rpc_without_session_id_not_matched() {
        let error = ProtocolError::Rpc {
            code: -32000,
            message: "Server not initialized".to_string(),
            data: None,
        };
        assert!(!is_terminated_session(&error, false));
    }
}
