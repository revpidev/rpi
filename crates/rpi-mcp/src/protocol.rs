//! JSON-RPC 2.0 message model and errors (port of
//! `packages/mcp/src/protocol/jsonrpc.ts` @ a13d35a74).

use std::fmt;
use std::hash::{Hash, Hasher};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `JSON_RPC_ERROR_CODES` (jsonrpc.ts:24).
pub const JSON_RPC_ERROR_PARSE: i64 = -32700;
pub const JSON_RPC_ERROR_INVALID_REQUEST: i64 = -32600;
pub const JSON_RPC_ERROR_METHOD_NOT_FOUND: i64 = -32601;
pub const JSON_RPC_ERROR_INVALID_PARAMS: i64 = -32602;
pub const JSON_RPC_ERROR_INTERNAL: i64 = -32603;

/// `JsonRpcId` (jsonrpc.ts:1): a string or a finite number. Numbers keep
/// their exact JSON representation (integers stay integers).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcId {
    Number(serde_json::Number),
    String(String),
}

impl Hash for JsonRpcId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            // Numbers hash through their canonical JSON text; `Number` is not
            // `Hash` because of its float representation.
            JsonRpcId::Number(number) => number.to_string().hash(state),
            JsonRpcId::String(value) => value.hash(state),
        }
    }
}

impl fmt::Display for JsonRpcId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JsonRpcId::Number(number) => write!(formatter, "{number}"),
            JsonRpcId::String(value) => write!(formatter, "{value}"),
        }
    }
}

/// `isJsonRpcId` (jsonrpc.ts:69): strings, and finite numbers.
pub fn is_json_rpc_id(value: &Value) -> bool {
    match value {
        Value::String(_) => true,
        Value::Number(number) => number.as_f64().is_some_and(f64::is_finite),
        _ => false,
    }
}

/// Parse a JSON value into a [`JsonRpcId`] (caller checked
/// [`is_json_rpc_id`] first).
pub fn parse_json_rpc_id(value: &Value) -> Option<JsonRpcId> {
    match value {
        Value::String(text) => Some(JsonRpcId::String(text.clone())),
        Value::Number(number) if number.as_f64().is_some_and(f64::is_finite) => {
            Some(JsonRpcId::Number(number.clone()))
        }
        _ => None,
    }
}

/// `isObject` (jsonrpc.ts:60): a JSON object (not an array, not null).
pub fn is_object(value: &Value) -> bool {
    value.is_object()
}

/// `JsonRpcErrorObject` (jsonrpc.ts:14).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcErrorObject {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// A classified JSON-RPC message (`parseJsonRpcMessage`, jsonrpc.ts:91).
#[derive(Debug, Clone, PartialEq)]
pub enum JsonRpcMessage {
    /// A request from either side (`id` + `method`). `params` is `Null` when absent.
    Request {
        id: JsonRpcId,
        method: String,
        params: Value,
    },
    /// A notification (`method` without `id`).
    Notification { method: String, params: Value },
    /// A response, either success (`result`) or failure (`error`).
    Response {
        id: JsonRpcId,
        result: Option<Value>,
        error: Option<JsonRpcErrorObject>,
    },
}

/// `isJsonRpcRequest` (jsonrpc.ts:73).
pub fn is_json_rpc_request(message: &Value) -> bool {
    is_object(message)
        && message.get("jsonrpc") == Some(&Value::String("2.0".to_owned()))
        && message.get("id").is_some_and(is_json_rpc_id)
        && message.get("method").is_some_and(Value::is_string)
}

/// `isJsonRpcNotification` (jsonrpc.ts:81).
pub fn is_json_rpc_notification(message: &Value) -> bool {
    is_object(message)
        && message.get("jsonrpc") == Some(&Value::String("2.0".to_owned()))
        && !message
            .as_object()
            .is_some_and(|map| map.contains_key("id"))
        && message.get("method").is_some_and(Value::is_string)
}

/// `isJsonRpcResponse` (jsonrpc.ts:84).
pub fn is_json_rpc_response(message: &Value) -> bool {
    if !is_object(message)
        || message.get("jsonrpc") != Some(&Value::String("2.0".to_owned()))
        || !message.get("id").is_some_and(is_json_rpc_id)
    {
        return false;
    }
    let map = message.as_object().expect("checked object");
    if map.contains_key("result") {
        return !map.contains_key("error");
    }
    let Some(error) = map.get("error").and_then(Value::as_object) else {
        return false;
    };
    error.get("code").is_some_and(Value::is_number)
        && error.get("message").is_some_and(Value::is_string)
}

/// `parseJsonRpcMessage` (jsonrpc.ts:91). `None` (the upstream
/// `McpError(invalidRequest)`) for anything unrecognized.
pub fn parse_json_rpc_message(message: Value) -> Option<JsonRpcMessage> {
    if is_json_rpc_response(&message) {
        let map = message.as_object().expect("checked object");
        let id = parse_json_rpc_id(map.get("id").expect("checked id"))?;
        let error = match map.get("error") {
            Some(error) if !error.is_null() => Some(JsonRpcErrorObject {
                code: error.get("code").and_then(Value::as_i64)?,
                message: error.get("message").and_then(Value::as_str)?.to_owned(),
                data: error.get("data").cloned().filter(|data| !data.is_null()),
            }),
            _ => None,
        };
        return Some(JsonRpcMessage::Response {
            id,
            result: if error.is_none() {
                Some(map.get("result").cloned().unwrap_or(Value::Null))
            } else {
                None
            },
            error,
        });
    }
    if is_json_rpc_request(&message) || is_json_rpc_notification(&message) {
        let map = message.as_object().expect("checked object");
        let params = map.get("params").cloned().unwrap_or(Value::Null);
        if let Some(id) = map.get("id").and_then(parse_json_rpc_id) {
            return Some(JsonRpcMessage::Request {
                id,
                method: map.get("method").and_then(Value::as_str)?.to_owned(),
                params,
            });
        }
        return Some(JsonRpcMessage::Notification {
            method: map.get("method").and_then(Value::as_str)?.to_owned(),
            params,
        });
    }
    None
}

/// `McpError`: JSON-RPC errors plus the client-level failures the ported
/// surface distinguishes. Transport failures keep their classification so
/// the connection runtime can retry, prompt for sign-in, or reconnect.
#[derive(Debug, Clone, thiserror::Error)]
pub enum McpError {
    /// A JSON-RPC error response (`McpError`, jsonrpc.ts:32).
    #[error("{message}")]
    Rpc {
        code: i64,
        message: String,
        data: Option<Value>,
    },
    /// `McpConnectionClosedError` (jsonrpc.ts:43).
    #[error("MCP connection closed{0}")]
    ConnectionClosed(String),
    /// `McpTimeoutError` (jsonrpc.ts:50).
    #[error("MCP request timed out after {timeout_ms}ms")]
    Timeout { timeout_ms: u64 },
    /// `McpAbortError` (jsonrpc.ts:56).
    #[error("MCP request aborted")]
    Aborted,
    /// Transport failures without an HTTP status (protocol/IO).
    #[error("{0}")]
    Transport(String),
    /// Local network failures (upstream `TypeError` from fetch), retryable.
    #[error("{0}")]
    Network(String),
    /// A non-auth HTTP failure (`McpHttpError`).
    #[error("MCP HTTP request failed with status {status}: {message}")]
    HttpError { status: u16, message: String },
    /// `McpAuthRequiredError` 401 (or 403 asking for more scope).
    #[error("MCP server requires authentication")]
    AuthRequired,
    /// `McpOAuthAuthorizationRequiredError`: the user has to sign in again.
    #[error("MCP OAuth authorization requires user interaction")]
    AuthorizationRequired,
    /// `McpSessionExpiredError` (404 after a session id was issued).
    #[error("MCP session expired")]
    SessionExpired,
    /// Invalid data or call shape (upstream `invalid(...)`).
    #[error("{0}")]
    Invalid(String),
}

impl McpError {
    /// JSON-RPC error code when this is a server error.
    pub fn rpc_code(&self) -> Option<i64> {
        match self {
            McpError::Rpc { code, .. } => Some(*code),
            _ => None,
        }
    }

    /// JSON-RPC error data when this is a server error.
    pub fn rpc_data(&self) -> Option<&Value> {
        match self {
            McpError::Rpc { data, .. } => data.as_ref(),
            _ => None,
        }
    }

    /// Network failures and overloaded or restarting servers, which are
    /// worth another attempt (runtime.ts:53).
    pub fn is_transient(&self) -> bool {
        match self {
            McpError::Network(_) => true,
            McpError::HttpError { status, .. } => {
                *status == 408 || *status == 429 || (*status >= 500 && *status != 501)
            }
            _ => false,
        }
    }

    /// The connection runtime's `needsSignIn` (runtime.ts:398): a server that
    /// still rejects the request after a refresh, or an OAuth server whose
    /// stored credentials are gone.
    pub fn is_authorization_required(&self) -> bool {
        matches!(self, McpError::AuthorizationRequired)
    }

    pub fn is_http_error(&self) -> bool {
        matches!(self, McpError::HttpError { .. })
    }

    pub(crate) fn connection_closed() -> Self {
        McpError::ConnectionClosed(String::new())
    }
}

/// `toError` (jsonrpc.ts:64) for `&str`-shaped failures.
pub fn error_message(error: impl fmt::Display) -> String {
    error.to_string()
}

impl JsonRpcMessage {
    /// Serialize back to the wire shape (params omitted when absent), for
    /// logging and tests.
    pub fn to_json(&self) -> Value {
        match self {
            JsonRpcMessage::Request { id, method, params } => {
                let mut message = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": serde_json::to_value(id).unwrap_or(Value::Null),
                    "method": method,
                });
                if !params.is_null() {
                    message["params"] = params.clone();
                }
                message
            }
            JsonRpcMessage::Notification { method, params } => {
                let mut message = serde_json::json!({"jsonrpc": "2.0", "method": method});
                if !params.is_null() {
                    message["params"] = params.clone();
                }
                message
            }
            JsonRpcMessage::Response { id, result, error } => {
                let mut message = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": serde_json::to_value(id).unwrap_or(Value::Null),
                });
                if let Some(error) = error {
                    message["error"] = serde_json::json!({
                        "code": error.code,
                        "message": error.message,
                        "data": error.data,
                    });
                } else {
                    message["result"] = result.clone().unwrap_or(Value::Null);
                }
                message
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn classifies_messages_like_upstream() {
        let request = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"});
        assert!(is_json_rpc_request(&request));
        assert!(!is_json_rpc_notification(&request));
        assert!(!is_json_rpc_response(&request));

        let notification = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        assert!(is_json_rpc_notification(&notification));
        assert!(!is_json_rpc_request(&notification));

        let success = json!({"jsonrpc": "2.0", "id": "a", "result": {}});
        assert!(is_json_rpc_response(&success));
        let failure =
            json!({"jsonrpc": "2.0", "id": 2, "error": {"code": -32601, "message": "nope"}});
        assert!(is_json_rpc_response(&failure));
        // A response carrying both result and error is invalid.
        let both =
            json!({"jsonrpc": "2.0", "id": 2, "result": 1, "error": {"code": 1, "message": "x"}});
        assert!(!is_json_rpc_response(&both));
        // An error without a message is invalid.
        let bad_error = json!({"jsonrpc": "2.0", "id": 2, "error": {"code": 1}});
        assert!(!is_json_rpc_response(&bad_error));
    }

    #[test]
    fn parses_each_message_shape() {
        let message =
            parse_json_rpc_message(json!({"jsonrpc": "2.0", "id": 3, "method": "ping"})).unwrap();
        assert_eq!(
            message,
            JsonRpcMessage::Request {
                id: JsonRpcId::Number(3.into()),
                method: "ping".to_owned(),
                params: Value::Null,
            }
        );

        let message =
            parse_json_rpc_message(json!({"jsonrpc": "2.0", "method": "note", "params": {"a": 1}}))
                .unwrap();
        assert_eq!(
            message,
            JsonRpcMessage::Notification {
                method: "note".to_owned(),
                params: json!({"a": 1}),
            }
        );

        let message =
            parse_json_rpc_message(json!({"jsonrpc": "2.0", "id": "x", "result": null})).unwrap();
        assert_eq!(
            message,
            JsonRpcMessage::Response {
                id: JsonRpcId::String("x".to_owned()),
                result: Some(Value::Null),
                error: None,
            }
        );

        let message = parse_json_rpc_message(
            json!({"jsonrpc": "2.0", "id": 4, "error": {"code": -32602, "message": "bad params", "data": [1]}}),
        )
        .unwrap();
        assert_eq!(
            message,
            JsonRpcMessage::Response {
                id: JsonRpcId::Number(4.into()),
                result: None,
                error: Some(JsonRpcErrorObject {
                    code: -32602,
                    message: "bad params".to_owned(),
                    data: Some(json!([1])),
                }),
            }
        );

        assert!(parse_json_rpc_message(json!({"id": 1, "method": "x"})).is_none());
        assert!(parse_json_rpc_message(json!({"jsonrpc": "2.0"})).is_none());
    }

    #[test]
    fn id_display_matches_json() {
        assert_eq!(JsonRpcId::Number(12.into()).to_string(), "12");
        assert_eq!(JsonRpcId::String("abc".to_owned()).to_string(), "abc");
    }
}
