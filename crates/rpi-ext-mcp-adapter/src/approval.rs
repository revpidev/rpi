//! approveTools: per-call approval gate emitting the
//! `pi-mcp-adapter:tool-approval-request` event (FR-P1-07).
//!
//! Port of `tool-approval.ts` @ 3d953f90.
//!
//! Headless fail-closed: when no UI handler claims the request and the
//! server's `approveTools` setting matches, the call is rejected with
//! `approval_required` (not allowed through).

use std::collections::HashSet;

use serde_json::{json, Value};

use crate::metadata::{
    get_tool_name_candidates, matches_tool_pattern, resolve_tool_prefix, McpConfig, ToolMetadata,
};

/// `MCP_TOOL_APPROVAL_REQUEST_EVENT` (types.ts:460).
pub const MCP_TOOL_APPROVAL_REQUEST_EVENT: &str = "pi-mcp-adapter:tool-approval-request";

/// `McpToolApprovalDecision` (types.ts:463).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    AllowOnce,
    AllowForSession,
    Deny,
    Abstain,
}

/// `McpToolApprovalOrigin` (types.ts:462). P1-wave scope: `script` and
/// `iframe` origins are P2; only `proxy`, `direct`, `resource` are used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalOrigin {
    Proxy,
    Direct,
    Resource,
}

/// `ToolCallApprovalResult` (tool-approval.ts:19-21).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolCallApprovalResult {
    Ok,
    Denied,
    ApprovalRequiredHeadless,
}

/// `isToolCallApprovalRequired` (tool-approval.ts:23-41): checks per-server
/// then global `approveTools` setting (glob pattern, same as include/exclude).
pub fn is_tool_call_approval_required(
    config: &McpConfig,
    server_name: &str,
    original_tool_name: &str,
) -> bool {
    let definition = config.mcp_servers.get(server_name);
    let approval = definition
        .and_then(|d| d.get("approveTools"))
        .or_else(|| config.settings.as_ref().and_then(|s| s.get("approveTools")));

    match approval {
        Some(Value::Bool(true)) => true,
        Some(Value::Array(list)) if !list.is_empty() => {
            let prefix = resolve_tool_prefix(definition, config.global_tool_prefix());
            let candidates = get_tool_name_candidates(original_tool_name, server_name, prefix);
            matches_tool_pattern(&candidates, Some(&Value::Array(list.clone())))
        }
        _ => false,
    }
}

/// Session-scoped approval cache (upstream `state.approvedToolCalls`).
/// Key: `server_name\0original_tool_name`.
#[derive(Default)]
pub struct ApprovalCache {
    approved: std::sync::Mutex<HashSet<String>>,
}

impl ApprovalCache {
    pub fn new() -> Self {
        Self::default()
    }

    fn cache_key(server_name: &str, original_tool_name: &str) -> String {
        format!("{server_name}\u{0}{original_tool_name}")
    }

    /// Returns `true` when this (server, tool) pair was already approved for
    /// the session (upstream `approvedToolCalls.has(cacheKey)`).
    pub fn is_approved(&self, server_name: &str, original_tool_name: &str) -> bool {
        let key = Self::cache_key(server_name, original_tool_name);
        self.approved
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&key)
    }

    /// Record a session-scoped approval (upstream `approvedToolCalls.set`).
    pub fn grant_session(&self, server_name: &str, original_tool_name: &str) {
        let key = Self::cache_key(server_name, original_tool_name);
        self.approved
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key);
    }

    /// Clear all session approvals.
    pub fn clear(&self) {
        self.approved
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
}

/// A handler that decides tool approval. In TUI mode this is backed by a
/// `ui.select` dialog; in headless mode no handler is registered so
/// `ensure_tool_call_approved` returns fail-closed.
pub trait ApprovalHandler: Send + Sync {
    fn decide(
        &self,
        server_name: &str,
        tool: &ToolMetadata,
        args: &Value,
        origin: ApprovalOrigin,
    ) -> ApprovalDecision;
}

/// `ensureToolCallApproved` (tool-approval.ts:90-141).
///
/// P1-wave cut: the broker-approval event path is simplified to a direct
/// handler trait — the upstream emits an event and any handler can `claim`
/// it; here we call the handler directly. When no handler is registered
/// (headless), the function returns `ApprovalRequiredHeadless` if the
/// server requires approval.
pub fn ensure_tool_call_approved(
    config: &McpConfig,
    cache: &ApprovalCache,
    handler: Option<&dyn ApprovalHandler>,
    server_name: &str,
    tool: &ToolMetadata,
    args: &Value,
    origin: ApprovalOrigin,
) -> ToolCallApprovalResult {
    // Session-scoped fast path.
    if cache.is_approved(server_name, &tool.original_name) {
        return ToolCallApprovalResult::Ok;
    }

    // Broker: if a handler is registered, ask it.
    if let Some(handler) = handler {
        match handler.decide(server_name, tool, args, origin) {
            ApprovalDecision::AllowOnce => return ToolCallApprovalResult::Ok,
            ApprovalDecision::AllowForSession => {
                cache.grant_session(server_name, &tool.original_name);
                return ToolCallApprovalResult::Ok;
            }
            ApprovalDecision::Deny => return ToolCallApprovalResult::Denied,
            ApprovalDecision::Abstain => {}
        }
    }

    // No handler claimed: check if approval is required.
    if !is_tool_call_approval_required(config, server_name, &tool.original_name) {
        return ToolCallApprovalResult::Ok;
    }

    // Headless fail-closed: no UI → rejection.
    ToolCallApprovalResult::ApprovalRequiredHeadless
}

/// Build the details object for an approval rejection result.
pub fn approval_rejection_details(
    result: &ToolCallApprovalResult,
    server_name: &str,
    original_tool_name: &str,
) -> (Value, Value) {
    match result {
        ToolCallApprovalResult::Denied => {
            let message = format!(
                "The user declined approval to run MCP tool \"{original_tool_name}\" on server \"{server_name}\"."
            );
            (
                json!([{"type": "text", "text": message}]),
                json!({
                    "error": "approval_denied",
                    "server": server_name,
                    "tool": original_tool_name,
                }),
            )
        }
        ToolCallApprovalResult::ApprovalRequiredHeadless => {
            let message = format!(
                "MCP tool \"{original_tool_name}\" on server \"{server_name}\" is approval-gated and requires an interactive session."
            );
            (
                json!([{"type": "text", "text": message}]),
                json!({
                    "error": "approval_required",
                    "server": server_name,
                    "tool": original_tool_name,
                }),
            )
        }
        ToolCallApprovalResult::Ok => (json!([]), json!({})),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::ServerEntry;
    use serde_json::json;

    fn config_with_approval(approve: Option<Value>) -> McpConfig {
        let mut config = McpConfig::default();
        let mut entry = serde_json::Map::new();
        entry.insert("command".to_string(), json!("node"));
        if let Some(a) = approve {
            entry.insert("approveTools".to_string(), a);
        }
        config
            .mcp_servers
            .insert("demo".to_string(), ServerEntry(entry));
        config
    }

    fn tool() -> ToolMetadata {
        ToolMetadata {
            name: "demo_search".to_string(),
            original_name: "search".to_string(),
            description: "Search things".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn approval_not_required_when_unset() {
        let config = config_with_approval(None);
        assert!(!is_tool_call_approval_required(&config, "demo", "search"));
    }

    #[test]
    fn approval_required_when_true() {
        let config = config_with_approval(Some(json!(true)));
        assert!(is_tool_call_approval_required(&config, "demo", "search"));
    }

    #[test]
    fn approval_required_by_glob() {
        // `demo_*` matches the prefixed candidate `demo_search`.
        let config = config_with_approval(Some(json!(["demo_*"])));
        assert!(is_tool_call_approval_required(&config, "demo", "search"));

        // `*search*` matches any candidate containing "search".
        let config = config_with_approval(Some(json!(["*search*"])));
        assert!(is_tool_call_approval_required(&config, "demo", "search"));
    }

    #[test]
    fn approval_not_required_by_non_matching_glob() {
        let config = config_with_approval(Some(json!(["other_*"])));
        assert!(!is_tool_call_approval_required(&config, "demo", "search"));
    }

    #[test]
    fn headless_fail_closed_when_approval_required() {
        let config = config_with_approval(Some(json!(true)));
        let cache = ApprovalCache::new();
        let result = ensure_tool_call_approved(
            &config,
            &cache,
            None,
            "demo",
            &tool(),
            &json!({}),
            ApprovalOrigin::Proxy,
        );
        assert_eq!(result, ToolCallApprovalResult::ApprovalRequiredHeadless);
    }

    #[test]
    fn headless_passes_when_not_required() {
        let config = config_with_approval(None);
        let cache = ApprovalCache::new();
        let result = ensure_tool_call_approved(
            &config,
            &cache,
            None,
            "demo",
            &tool(),
            &json!({}),
            ApprovalOrigin::Proxy,
        );
        assert_eq!(result, ToolCallApprovalResult::Ok);
    }

    struct AlwaysDeny;
    impl ApprovalHandler for AlwaysDeny {
        fn decide(
            &self,
            _server: &str,
            _tool: &ToolMetadata,
            _args: &Value,
            _origin: ApprovalOrigin,
        ) -> ApprovalDecision {
            ApprovalDecision::Deny
        }
    }

    #[test]
    fn handler_deny_rejected() {
        let config = config_with_approval(Some(json!(true)));
        let cache = ApprovalCache::new();
        let handler = AlwaysDeny;
        let result = ensure_tool_call_approved(
            &config,
            &cache,
            Some(&handler),
            "demo",
            &tool(),
            &json!({}),
            ApprovalOrigin::Direct,
        );
        assert_eq!(result, ToolCallApprovalResult::Denied);
    }

    struct AllowForSession;
    impl ApprovalHandler for AllowForSession {
        fn decide(
            &self,
            _server: &str,
            _tool: &ToolMetadata,
            _args: &Value,
            _origin: ApprovalOrigin,
        ) -> ApprovalDecision {
            ApprovalDecision::AllowForSession
        }
    }

    #[test]
    fn handler_allow_for_session_cached() {
        let config = config_with_approval(Some(json!(true)));
        let cache = ApprovalCache::new();
        let handler = AllowForSession;

        // First call: handler grants session approval
        let result = ensure_tool_call_approved(
            &config,
            &cache,
            Some(&handler),
            "demo",
            &tool(),
            &json!({}),
            ApprovalOrigin::Proxy,
        );
        assert_eq!(result, ToolCallApprovalResult::Ok);

        // Second call: cache hits (no handler needed)
        let result2 = ensure_tool_call_approved(
            &config,
            &cache,
            None,
            "demo",
            &tool(),
            &json!({}),
            ApprovalOrigin::Proxy,
        );
        assert_eq!(result2, ToolCallApprovalResult::Ok);
    }

    #[test]
    fn global_settings_approve_tools_fallback() {
        let mut config = McpConfig {
            settings: Some(
                json!({ "approveTools": true })
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            ),
            ..Default::default()
        };
        config.mcp_servers.insert(
            "demo".to_string(),
            ServerEntry(
                json!({ "command": "x" })
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            ),
        );
        assert!(is_tool_call_approval_required(&config, "demo", "search"));
    }

    #[test]
    fn per_server_overrides_global() {
        let mut config = McpConfig {
            settings: Some(
                json!({ "approveTools": true })
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            ),
            ..Default::default()
        };
        let mut entry = serde_json::Map::new();
        entry.insert("command".to_string(), json!("node"));
        // Per-server false overrides global true? No — upstream code checks
        // `definition.approveTools !== undefined ? definition.approveTools :
        // settings.approveTools`, so `false` means approval IS explicitly off.
        entry.insert("approveTools".to_string(), json!(false));
        config
            .mcp_servers
            .insert("demo".to_string(), ServerEntry(entry));
        assert!(!is_tool_call_approval_required(&config, "demo", "search"));
    }
}
