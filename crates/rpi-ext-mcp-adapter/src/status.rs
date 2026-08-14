//! Status surface: footer status (full/compact/off) + versioned event-bus
//! snapshot (FR-P1-09).
//!
//! Port of `mcp-status.ts` @ 3d953f90: `createMcpStatusSnapshot` /
//! `publishMcpStatusSnapshot` / `publishMcpStatusShutdown`.
//!
//! The snapshot is read-only and does NOT trigger connections — it reads
//! existing connection and metadata state (design requirement).

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::manager::{ConnectionStatus, McpServerManager};
use crate::metadata::McpConfig;

/// `MCP_STATUS_EVENT` (types.ts:16): versioned event channel.
pub const MCP_STATUS_EVENT: &str = "pi-mcp-adapter/status/v1";

/// `MCP_STATUS_SNAPSHOT_VERSION` (types.ts:18).
pub const MCP_STATUS_SNAPSHOT_VERSION: u32 = 1;

/// `McpServerRuntimeStatus` (types.ts:20-26).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ServerRuntimeStatus {
    Connected,
    Cached,
    Failed,
    #[serde(rename = "needs-auth")]
    NeedsAuth,
    #[serde(rename = "not-connected")]
    NotConnected,
    Disabled,
}

/// `McpServerStatusSnapshot` (types.ts:28-35).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerStatusSnapshot {
    pub name: String,
    pub status: ServerRuntimeStatus,
    pub tool_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed_ago_seconds: Option<u64>,
    pub disabled: bool,
}

/// `McpStatusSnapshot` (types.ts:37-44).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpStatusSnapshot {
    pub version: u32,
    pub servers: Vec<ServerStatusSnapshot>,
    pub total_tools: u64,
    pub total_resources: u64,
    pub connected_count: u64,
    pub disabled_count: u64,
}

/// `mcpFooterStatus` setting (types.ts / settings).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FooterStatus {
    Full,
    Compact,
    #[default]
    Off,
}

impl FooterStatus {
    pub fn from_settings(settings: Option<&serde_json::Map<String, Value>>) -> Self {
        match settings
            .and_then(|s| s.get("mcpFooterStatus"))
            .and_then(Value::as_str)
        {
            Some("full") => FooterStatus::Full,
            Some("compact") => FooterStatus::Compact,
            Some("off") => FooterStatus::Off,
            _ => FooterStatus::default(),
        }
    }
}

/// The failure backoff (mcp-status.ts:9): 60s.
const FAILURE_BACKOFF_MS: u64 = 60_000;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Build the status footer text for the UI (mcp-status.ts + proxy.ts
/// `execute_status` footer line).
pub fn build_footer_text(snapshot: &McpStatusSnapshot, mode: FooterStatus) -> Option<String> {
    match mode {
        FooterStatus::Off => None,
        FooterStatus::Full => Some(format!(
            "MCP: {}/{} servers, {} tools",
            snapshot.connected_count,
            snapshot.servers.len() as u64 - snapshot.disabled_count,
            snapshot.total_tools
        )),
        FooterStatus::Compact => {
            if snapshot.connected_count == 0 {
                None
            } else {
                Some(format!("MCP: {} tools", snapshot.total_tools))
            }
        }
    }
}

/// `createMcpStatusSnapshot` (mcp-status.ts:24-77): build a sanitized
/// snapshot without connecting or querying any MCP server.
///
/// Parameters:
/// - `config`: the merged MCP config
/// - `manager`: the connection manager (read-only `get_connection`)
/// - `tool_metadata`: per-server metadata (server_name → tool count)
/// - `failure_tracker`: per-server failure timestamps (server_name →
///   unix-millis when the failure was recorded)
pub fn create_mcp_status_snapshot(
    config: &McpConfig,
    manager: &McpServerManager,
    tool_metadata: &[(String, usize)],
    resource_counts: &[(String, usize)],
    failure_tracker: &[(String, u64)],
) -> McpStatusSnapshot {
    let mut servers = Vec::new();
    let mut total_tools = 0u64;
    let mut total_resources = 0u64;
    let mut connected_count = 0u64;
    let mut disabled_count = 0u64;

    let tool_meta_count = |name: &str| -> usize {
        tool_metadata
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, c)| *c)
            .unwrap_or(0)
    };
    let resource_count = |name: &str| -> Option<usize> {
        resource_counts
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, c)| *c)
    };
    let failure_age = |name: &str| -> Option<u64> {
        let failed_at = failure_tracker
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, t)| *t)?;
        let age_ms = now_secs().saturating_mul(1000).saturating_sub(failed_at);
        if age_ms > FAILURE_BACKOFF_MS {
            return None;
        }
        Some(age_ms / 1000)
    };

    for (name, definition) in &config.mcp_servers {
        let disabled = definition.is_disabled();
        let connection = if disabled {
            None
        } else {
            manager.get_connection(name)
        };

        let tool_count = if disabled {
            0
        } else {
            let meta = tool_meta_count(name);
            if meta > 0 {
                meta
            } else if connection
                .as_ref()
                .is_some_and(|c| c.status() == ConnectionStatus::Connected)
            {
                connection.as_ref().map(|c| c.tools.len()).unwrap_or(0)
            } else {
                0
            }
        };

        let res_count = if disabled {
            None
        } else {
            let rc = resource_count(name);
            if rc.is_some() {
                rc
            } else if connection
                .as_ref()
                .is_some_and(|c| c.status() == ConnectionStatus::Connected)
            {
                Some(connection.as_ref().map(|c| c.resources.len()).unwrap_or(0))
            } else {
                None
            }
        };

        let failed_ago = if disabled { None } else { failure_age(name) };

        let status = if disabled {
            disabled_count += 1;
            ServerRuntimeStatus::Disabled
        } else if connection
            .as_ref()
            .is_some_and(|c| c.status() == ConnectionStatus::Connected)
        {
            connected_count += 1;
            ServerRuntimeStatus::Connected
        } else if connection
            .as_ref()
            .is_some_and(|c| c.status() == ConnectionStatus::NeedsAuth)
        {
            ServerRuntimeStatus::NeedsAuth
        } else if failed_ago.is_some() {
            ServerRuntimeStatus::Failed
        } else if tool_count > 0 {
            ServerRuntimeStatus::Cached
        } else {
            ServerRuntimeStatus::NotConnected
        };

        total_tools += if disabled { 0 } else { tool_count as u64 };
        if let Some(rc) = res_count {
            total_resources += rc as u64;
        }

        servers.push(ServerStatusSnapshot {
            name: name.clone(),
            status,
            tool_count: tool_count as u64,
            resource_count: res_count.map(|c| c as u64),
            failed_ago_seconds: failed_ago,
            disabled,
        });
    }

    McpStatusSnapshot {
        version: MCP_STATUS_SNAPSHOT_VERSION,
        servers,
        total_tools,
        total_resources,
        connected_count,
        disabled_count,
    }
}

/// Shutdown snapshot (mcp-status.ts:92-106): empty servers, zeroed counts.
pub fn shutdown_snapshot() -> McpStatusSnapshot {
    McpStatusSnapshot {
        version: MCP_STATUS_SNAPSHOT_VERSION,
        servers: Vec::new(),
        total_tools: 0,
        total_resources: 0,
        connected_count: 0,
        disabled_count: 0,
    }
}

/// Serialize a snapshot to JSON for the event bus.
pub fn snapshot_to_json(snapshot: &McpStatusSnapshot) -> Value {
    serde_json::to_value(snapshot).unwrap_or(json!({}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn snapshot_shape_is_versioned() {
        let snapshot = shutdown_snapshot();
        assert_eq!(snapshot.version, 1);
        assert!(snapshot.servers.is_empty());
        assert_eq!(snapshot.total_tools, 0);
    }

    #[test]
    fn footer_modes() {
        let snapshot = McpStatusSnapshot {
            version: 1,
            servers: vec![ServerStatusSnapshot {
                name: "demo".to_string(),
                status: ServerRuntimeStatus::Connected,
                tool_count: 5,
                resource_count: Some(2),
                failed_ago_seconds: None,
                disabled: false,
            }],
            total_tools: 5,
            total_resources: 2,
            connected_count: 1,
            disabled_count: 0,
        };

        // Full mode
        let text = build_footer_text(&snapshot, FooterStatus::Full).unwrap();
        assert!(text.contains("MCP: 1/1 servers, 5 tools"));

        // Compact mode
        let text = build_footer_text(&snapshot, FooterStatus::Compact).unwrap();
        assert_eq!(text, "MCP: 5 tools");

        // Off mode
        assert!(build_footer_text(&snapshot, FooterStatus::Off).is_none());
    }

    #[test]
    fn footer_compact_hidden_when_no_connections() {
        let snapshot = McpStatusSnapshot {
            version: 1,
            servers: vec![],
            total_tools: 0,
            total_resources: 0,
            connected_count: 0,
            disabled_count: 0,
        };
        assert!(build_footer_text(&snapshot, FooterStatus::Compact).is_none());
    }

    #[test]
    fn footer_status_from_settings() {
        let settings = json!({ "mcpFooterStatus": "full" })
            .as_object()
            .cloned()
            .unwrap_or_default();
        assert_eq!(
            FooterStatus::from_settings(Some(&settings)),
            FooterStatus::Full
        );

        let settings = json!({ "mcpFooterStatus": "compact" })
            .as_object()
            .cloned()
            .unwrap_or_default();
        assert_eq!(
            FooterStatus::from_settings(Some(&settings)),
            FooterStatus::Compact
        );

        assert_eq!(FooterStatus::from_settings(None), FooterStatus::Off);
    }

    #[test]
    fn snapshot_json_serializes_correctly() {
        let snapshot = shutdown_snapshot();
        let json_val = snapshot_to_json(&snapshot);
        assert_eq!(json_val["version"], json!(1));
        assert!(json_val["servers"].is_array());
    }
}
