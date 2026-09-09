//! approveTools: per-call approval gate emitting the
//! `pi-mcp-adapter:tool-approval-request` event (FR-P1-07, R7.2.2).
//!
//! Port of `tool-approval.ts` @ `10a45367` (v2.32.1: argument-scoped cache
//! key + legacy-candidate exclusion, upstream #367) and `@ 928c30c`
//! (session persistence, #505/#492 — see [`crate::session_approvals`]).
//!
//! Headless fail-closed: when no UI handler is available and the server's
//! `approveTools` setting matches, the call is rejected with
//! `approval_required` (not allowed through).

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use indexmap::IndexMap;
use serde_json::{json, Map, Value};

use crate::metadata::{
    get_tool_name_candidates_with, matches_tool_pattern_exact, resolve_tool_prefix, McpConfig,
    ToolMetadata,
};
use crate::session_approvals::{
    get_tool_approval_identity, restored_approval_keys, SessionApprovalEntry, SessionApprovalSink,
    ToolApprovalIdentity,
};

/// `MCP_TOOL_APPROVAL_REQUEST_EVENT` (types.ts:527 @ 10a45367).
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

/// `ToolCallApprovalResult` (tool-approval.ts:19-21 @ 10a45367).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolCallApprovalResult {
    Ok,
    Denied,
    ApprovalRequiredHeadless,
}

/// A handler that decides tool approval. The broker role mirrors the
/// upstream `pi-mcp-adapter:tool-approval-request` claim handler; the UI role
/// mirrors the built-in three-choice dialog (`ui.select`). In headless mode
/// both are absent so [`ensure_tool_call_approved`] returns fail-closed.
pub trait ApprovalHandler: Send + Sync {
    fn decide(
        &self,
        server_name: &str,
        tool: &ToolMetadata,
        args: &Value,
        origin: ApprovalOrigin,
    ) -> ApprovalDecision;
}

/// Session-scoped approval cache (upstream `state.approvedToolCalls`).
///
/// Key: `server\0originalTool\0definitionHash\0argsHash` (session-approvals.ts
/// `makeToolApprovalKey` @ 928c30c). Grants are persisted through the
/// [`SessionApprovalSink`] when one is bound (host `appendEntry`).
#[derive(Default)]
pub struct ApprovalCache {
    approved: Mutex<HashSet<String>>,
    sink: Mutex<Option<Arc<dyn SessionApprovalSink>>>,
}

impl ApprovalCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind the active-session writer (host `appendEntry`). Replaces any
    /// previous sink — rebind/`session_start` builds a fresh runtime.
    pub fn set_sink(&self, sink: Arc<dyn SessionApprovalSink>) {
        *self.sink.lock().unwrap_or_else(|e| e.into_inner()) = Some(sink);
    }

    /// Returns `true` when this identity was already approved for the
    /// session (upstream `approvedToolCalls.has(cacheKey)`).
    pub fn is_approved(&self, cache_key: &str) -> bool {
        self.approved
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(cache_key)
    }

    /// Record a session-scoped approval and persist it once. Returns `true`
    /// when the key was newly inserted (a cache hit never re-emits an entry,
    /// matching upstream `rememberToolApproval`).
    pub fn grant_session(
        &self,
        server_name: &str,
        original_tool_name: &str,
        identity: &ToolApprovalIdentity,
    ) -> bool {
        let newly_inserted = self
            .approved
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(identity.cache_key.clone());
        if !newly_inserted {
            return false;
        }
        let sink = self.sink.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(sink) = sink {
            let entry = SessionApprovalEntry::allow_for_session(
                server_name,
                original_tool_name,
                &identity.definition_hash,
                &identity.args_hash,
            );
            sink.append(&entry);
        }
        true
    }

    /// `restoreSessionApprovalState` (session-approvals.ts:178-199 @ 928c30c):
    /// clear, then replay the branch entries (idempotent rebuild).
    pub fn restore(&self, entries: &[SessionApprovalEntry]) {
        let keys = restored_approval_keys(entries);
        let mut approved = self.approved.lock().unwrap_or_else(|e| e.into_inner());
        approved.clear();
        approved.extend(keys);
    }

    /// Clear all session approvals.
    pub fn clear(&self) {
        self.approved
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    pub fn len(&self) -> usize {
        self.approved
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Sorted snapshot for diagnostics/tests.
    pub fn snapshot(&self) -> Vec<String> {
        let mut keys: Vec<String> = self
            .approved
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect();
        keys.sort();
        keys
    }
}

/// Upstream `otherCurrentCandidates` (tool-approval.ts:44-66 @ 10a45367):
/// the current-name candidates of every *other* tool in the selector's
/// scope, used to suppress a legacy-only glob that would sweep another tool.
#[derive(Debug, Clone, Default)]
pub struct ApprovalCandidateContext {
    pub other_current_candidates: Vec<String>,
}

/// Build the candidate context for one tool. Per-server `approveTools`
/// scopes the comparison to the same server; a global setting compares
/// across all servers (each with its own resolved prefix).
pub fn approval_candidate_context(
    config: &McpConfig,
    tool_metadata: &IndexMap<String, Vec<ToolMetadata>>,
    server_name: &str,
    original_tool_name: &str,
) -> ApprovalCandidateContext {
    let definition = config.mcp_servers.get(server_name);
    let server_scoped = definition.and_then(|d| d.get("approveTools")).is_some();
    let prefix = resolve_tool_prefix(definition, config.global_tool_prefix());
    let current = get_tool_name_candidates_with(original_tool_name, server_name, prefix, false);

    let mut other: Vec<String> = Vec::new();
    let mut push = |value: String| {
        if !other.contains(&value) {
            other.push(value);
        }
    };
    if server_scoped {
        if let Some(tools) = tool_metadata.get(server_name) {
            for tool in tools {
                for candidate in
                    get_tool_name_candidates_with(&tool.original_name, server_name, prefix, false)
                {
                    push(candidate);
                }
            }
        }
    } else {
        for (name, tools) in tool_metadata {
            let other_prefix =
                resolve_tool_prefix(config.mcp_servers.get(name), config.global_tool_prefix());
            for tool in tools {
                for candidate in
                    get_tool_name_candidates_with(&tool.original_name, name, other_prefix, false)
                {
                    push(candidate);
                }
            }
        }
    }
    other.retain(|candidate| !current.contains(candidate));
    ApprovalCandidateContext {
        other_current_candidates: other,
    }
}

/// `isToolCallApprovalRequired` (tool-approval.ts:23-72 @ 10a45367): checks
/// per-server then global `approveTools`. `context` carries the
/// `otherCurrentCandidates` index; `None` degrades to the upstream
/// no-metadata branches.
pub fn is_tool_call_approval_required(
    config: &McpConfig,
    server_name: &str,
    original_tool_name: &str,
    context: Option<&ApprovalCandidateContext>,
) -> bool {
    let definition = config.mcp_servers.get(server_name);
    let server_approval = definition.and_then(|d| d.get("approveTools"));
    let approval: Option<Value> = match server_approval {
        Some(value) => Some(value.clone()),
        None => config
            .settings
            .as_ref()
            .and_then(|settings| settings.get("approveTools"))
            .cloned(),
    };

    let Some(approval) = approval else {
        return false;
    };
    if approval == Value::Bool(true) {
        return true;
    }
    let Value::Array(patterns) = &approval else {
        return false;
    };
    if patterns.is_empty() {
        return false;
    }

    let prefix = resolve_tool_prefix(definition, config.global_tool_prefix());
    let current = get_tool_name_candidates_with(original_tool_name, server_name, prefix, false);
    if matches_tool_pattern_exact(&current, Some(&approval)) {
        return true;
    }

    // Upstream per-server no-metadata fallback: legacy candidates still gate.
    if server_approval.is_some() && context.is_none() {
        return matches_tool_pattern_exact(
            &get_tool_name_candidates_with(original_tool_name, server_name, prefix, true),
            Some(&approval),
        );
    }
    // Upstream global no-metadata branch: only current candidates gate.
    let Some(context) = context else {
        return false;
    };

    let mut legacy = get_tool_name_candidates_with(original_tool_name, server_name, prefix, true);
    if let Some(legacy_emitted_name) = current
        .iter()
        .find(|candidate| *candidate != original_tool_name)
    {
        let normalized = legacy_emitted_name.replace('-', "_");
        if !legacy.contains(&normalized) {
            legacy.push(normalized);
        }
    }
    legacy.retain(|candidate| !current.contains(candidate));

    patterns.iter().any(|pattern| {
        let single = Value::Array(vec![pattern.clone()]);
        matches_tool_pattern_exact(&legacy, Some(&single))
            && !matches_tool_pattern_exact(&context.other_current_candidates, Some(&single))
    })
}

/// `ensureToolCallApproved` (tool-approval.ts:104-165 @ 10a45367 + :118-124
/// persistence @ 928c30c).
///
/// `candidate_context` is evaluated lazily (after the cache/broker fast
/// paths) so an unconfigured server pays no metadata scan.
#[allow(clippy::too_many_arguments)] // upstream-shaped gate: config/cache/
                                     // identity/origin/broker/ui/context stay explicit rather than a bag struct.
pub fn ensure_tool_call_approved<F>(
    config: &McpConfig,
    cache: &ApprovalCache,
    server_name: &str,
    tool: &ToolMetadata,
    args: &Value,
    origin: ApprovalOrigin,
    broker: Option<&dyn ApprovalHandler>,
    ui: Option<&dyn ApprovalHandler>,
    candidate_context: F,
) -> ToolCallApprovalResult
where
    F: FnOnce() -> ApprovalCandidateContext,
{
    let identity = get_tool_approval_identity(server_name, tool, args);

    // Session-scoped fast path.
    if cache.is_approved(&identity.cache_key) {
        return ToolCallApprovalResult::Ok;
    }

    // Broker: if a handler is registered, ask it (upstream emits the request
    // event and any claimer decides; the native port calls the handler).
    if let Some(broker) = broker {
        match broker.decide(server_name, tool, args, origin) {
            ApprovalDecision::AllowOnce => return ToolCallApprovalResult::Ok,
            ApprovalDecision::AllowForSession => {
                cache.grant_session(server_name, &tool.original_name, &identity);
                return ToolCallApprovalResult::Ok;
            }
            ApprovalDecision::Deny => return ToolCallApprovalResult::Denied,
            ApprovalDecision::Abstain => {}
        }
    }

    let context = candidate_context();
    if !is_tool_call_approval_required(config, server_name, &tool.original_name, Some(&context)) {
        return ToolCallApprovalResult::Ok;
    }

    let Some(ui) = ui else {
        return ToolCallApprovalResult::ApprovalRequiredHeadless;
    };

    match ui.decide(server_name, tool, args, origin) {
        ApprovalDecision::AllowOnce => ToolCallApprovalResult::Ok,
        ApprovalDecision::AllowForSession => {
            cache.grant_session(server_name, &tool.original_name, &identity);
            ToolCallApprovalResult::Ok
        }
        ApprovalDecision::Deny | ApprovalDecision::Abstain => ToolCallApprovalResult::Denied,
    }
}

/// Build the content/details pair for an approval rejection result.
/// `mode` prefixes the ordered details object with `mode: "call"` for the
/// proxy surface (upstream `executeCall`); the direct surface omits it.
pub fn approval_rejection_details(
    result: &ToolCallApprovalResult,
    server_name: &str,
    original_tool_name: &str,
    mode: Option<&str>,
) -> (Value, Value) {
    match result {
        ToolCallApprovalResult::Denied => {
            let message = format!(
                "The user declined approval to run MCP tool \"{original_tool_name}\" on server \"{server_name}\"."
            );
            (
                json!([{"type": "text", "text": message}]),
                ordered_details(mode, "approval_denied", server_name, original_tool_name),
            )
        }
        ToolCallApprovalResult::ApprovalRequiredHeadless => {
            let message = format!(
                "MCP tool \"{original_tool_name}\" on server \"{server_name}\" is approval-gated and requires an interactive session."
            );
            (
                json!([{"type": "text", "text": message}]),
                ordered_details(mode, "approval_required", server_name, original_tool_name),
            )
        }
        ToolCallApprovalResult::Ok => (json!([]), json!({})),
    }
}

/// `JSON.stringify(args ?? {}, null, 2)` + `sanitizeTerminalText` + 500-char
/// truncation (tool-approval.ts:160-165 @ 928c30c). Null/absent arguments
/// render as `{}` (upstream `args ?? {}`).
pub fn dialog_preview(args: &Value) -> String {
    let normalized = if args.is_null() {
        json!({})
    } else {
        args.clone()
    };
    let json = serde_json::to_string_pretty(&normalized).unwrap_or_else(|_| "{}".to_string());
    let sanitized = crate::utils::sanitize_terminal_text(&json);
    if sanitized.chars().count() > 500 {
        let mut truncated: String = sanitized.chars().take(500).collect();
        truncated.push_str("...");
        truncated
    } else {
        sanitized
    }
}

fn ordered_details(
    mode: Option<&str>,
    error: &str,
    server_name: &str,
    original_tool_name: &str,
) -> Value {
    let mut details = Map::new();
    if let Some(mode) = mode {
        details.insert("mode".to_string(), json!(mode));
    }
    details.insert("error".to_string(), json!(error));
    details.insert("server".to_string(), json!(server_name));
    details.insert("tool".to_string(), json!(original_tool_name));
    Value::Object(details)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::ServerEntry;
    use serde_json::json;

    fn server_entry(approve: Option<Value>) -> ServerEntry {
        let mut entry = Map::new();
        entry.insert("command".to_string(), json!("node"));
        if let Some(approve) = approve {
            entry.insert("approveTools".to_string(), approve);
        }
        ServerEntry(entry)
    }

    fn config_with_approval(approve: Option<Value>) -> McpConfig {
        let mut config = McpConfig::default();
        config
            .mcp_servers
            .insert("demo".to_string(), server_entry(approve));
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

    fn metadata_for(entries: &[(&str, &[(&str, &str)])]) -> IndexMap<String, Vec<ToolMetadata>> {
        let mut map = IndexMap::new();
        for (server, tools) in entries {
            map.insert(
                (*server).to_string(),
                tools
                    .iter()
                    .map(|(original, name)| ToolMetadata {
                        name: (*name).to_string(),
                        original_name: (*original).to_string(),
                        description: String::new(),
                        ..Default::default()
                    })
                    .collect(),
            );
        }
        map
    }

    // ---- bool / glob configuration (existing TE03 expectations, adapted) ----

    #[test]
    fn approval_not_required_when_unset() {
        let config = config_with_approval(None);
        assert!(!is_tool_call_approval_required(
            &config, "demo", "search", None
        ));
    }

    #[test]
    fn approval_required_when_true() {
        let config = config_with_approval(Some(json!(true)));
        assert!(is_tool_call_approval_required(
            &config, "demo", "search", None
        ));
    }

    #[test]
    fn approval_required_by_glob() {
        // `demo_*` matches the prefixed candidate `demo_search`.
        let config = config_with_approval(Some(json!(["demo_*"])));
        assert!(is_tool_call_approval_required(
            &config, "demo", "search", None
        ));

        // `*search*` matches any candidate containing "search".
        let config = config_with_approval(Some(json!(["*search*"])));
        assert!(is_tool_call_approval_required(
            &config, "demo", "search", None
        ));
    }

    #[test]
    fn approval_not_required_by_non_matching_glob() {
        let config = config_with_approval(Some(json!(["other_*"])));
        assert!(!is_tool_call_approval_required(
            &config, "demo", "search", None
        ));
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
        config
            .mcp_servers
            .insert("demo".to_string(), server_entry(None));
        assert!(is_tool_call_approval_required(
            &config, "demo", "search", None
        ));
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
        // Per-server `false` means approval IS explicitly off (upstream
        // `definition.approveTools !== undefined` check).
        config
            .mcp_servers
            .insert("demo".to_string(), server_entry(Some(json!(false))));
        assert!(!is_tool_call_approval_required(
            &config, "demo", "search", None
        ));
    }

    // ---- #367 legacy-candidate exclusion (task A5; upstream
    // `__tests__/tool-approval.test.ts:91-161` @ 10a45367) ----

    #[test]
    fn gates_exact_global_selectors_without_applying_them_through_a_legacy_collision() {
        let mut config = McpConfig {
            settings: Some(
                json!({ "approveTools": ["my_2d_server_do_thing"] })
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            ),
            ..Default::default()
        };
        config
            .mcp_servers
            .insert("my-server".to_string(), server_entry(None));
        config
            .mcp_servers
            .insert("my_2d_server".to_string(), server_entry(None));
        let metadata = metadata_for(&[
            ("my-server", &[("do-thing", "my-server_do-thing")]),
            ("my_2d_server", &[("do_thing", "my_2d_server_do_thing")]),
        ]);

        let context = approval_candidate_context(&config, &metadata, "my-server", "do-thing");
        assert!(!is_tool_call_approval_required(
            &config,
            "my-server",
            "do-thing",
            Some(&context)
        ));
        let context = approval_candidate_context(&config, &metadata, "my_2d_server", "do_thing");
        assert!(is_tool_call_approval_required(
            &config,
            "my_2d_server",
            "do_thing",
            Some(&context)
        ));
    }

    #[test]
    fn matches_safe_server_scoped_normalized_approval_selectors() {
        let mut config = McpConfig::default();
        config.mcp_servers.insert(
            "my-server".to_string(),
            server_entry(Some(json!(["my_server_do_thing"]))),
        );
        let metadata = metadata_for(&[("my-server", &[("do_thing", "my-server_do_thing")])]);
        let context = approval_candidate_context(&config, &metadata, "my-server", "do_thing");
        assert!(is_tool_call_approval_required(
            &config,
            "my-server",
            "do_thing",
            Some(&context)
        ));
    }

    #[test]
    fn does_not_gate_a_same_server_legacy_collision() {
        let mut config = McpConfig {
            settings: Some(
                json!({ "approveTools": ["demo_search_records"] })
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            ),
            ..Default::default()
        };
        config
            .mcp_servers
            .insert("demo".to_string(), server_entry(None));
        let metadata = metadata_for(&[(
            "demo",
            &[
                ("search-records", "demo_search-records"),
                ("search_records", "demo_search_records"),
            ],
        )]);

        let context = approval_candidate_context(&config, &metadata, "demo", "search-records");
        assert!(!is_tool_call_approval_required(
            &config,
            "demo",
            "search-records",
            Some(&context)
        ));
        let context = approval_candidate_context(&config, &metadata, "demo", "search_records");
        assert!(is_tool_call_approval_required(
            &config,
            "demo",
            "search_records",
            Some(&context)
        ));
    }

    // ---- gate behaviour (A1/A2/A8/A10) ----

    struct FixedHandler(ApprovalDecision);

    impl ApprovalHandler for FixedHandler {
        fn decide(
            &self,
            _server: &str,
            _tool: &ToolMetadata,
            _args: &Value,
            _origin: ApprovalOrigin,
        ) -> ApprovalDecision {
            self.0
        }
    }

    #[derive(Default)]
    struct RecordingSink(Mutex<Vec<SessionApprovalEntry>>);

    impl SessionApprovalSink for RecordingSink {
        fn append(&self, entry: &SessionApprovalEntry) {
            self.0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(entry.clone());
        }
    }

    fn no_context() -> ApprovalCandidateContext {
        ApprovalCandidateContext::default()
    }

    /// A1/A2: one argument payload grants, a different payload re-prompts,
    /// the original payload is then cached (and the grant is persisted once).
    #[test]
    fn argument_scope_isolates_payloads_and_caches_grants() {
        let config = config_with_approval(Some(json!(true)));
        let cache = ApprovalCache::new();
        let sink = Arc::new(RecordingSink::default());
        cache.set_sink(sink.clone());
        let ui = FixedHandler(ApprovalDecision::AllowForSession);

        let args_a = json!({"query": "a"});
        let args_b = json!({"query": "b"});

        let first = ensure_tool_call_approved(
            &config,
            &cache,
            "demo",
            &tool(),
            &args_a,
            ApprovalOrigin::Proxy,
            None,
            Some(&ui),
            no_context,
        );
        assert_eq!(first, ToolCallApprovalResult::Ok);
        assert_eq!(sink.0.lock().unwrap().len(), 1, "grant persisted once");

        // A2: same payload is cached, the UI handler is not consulted.
        let again = ensure_tool_call_approved(
            &config,
            &cache,
            "demo",
            &tool(),
            &args_a,
            ApprovalOrigin::Proxy,
            None,
            None,
            no_context,
        );
        assert_eq!(again, ToolCallApprovalResult::Ok);

        // A1: a different payload must be re-confirmed (headless here → fail
        // closed, which proves the key missed).
        let other = ensure_tool_call_approved(
            &config,
            &cache,
            "demo",
            &tool(),
            &args_b,
            ApprovalOrigin::Proxy,
            None,
            None,
            no_context,
        );
        assert_eq!(other, ToolCallApprovalResult::ApprovalRequiredHeadless);
        assert_eq!(cache.len(), 1);
        assert_eq!(sink.0.lock().unwrap().len(), 1);
    }

    /// A10: headless matching calls fail closed; UI decisions map to the
    /// three upstream outcomes.
    #[test]
    fn headless_fail_closed_and_ui_decisions() {
        let config = config_with_approval(Some(json!(true)));
        let cache = ApprovalCache::new();

        let headless = ensure_tool_call_approved(
            &config,
            &cache,
            "demo",
            &tool(),
            &json!({}),
            ApprovalOrigin::Proxy,
            None,
            None,
            no_context,
        );
        assert_eq!(headless, ToolCallApprovalResult::ApprovalRequiredHeadless);

        let deny = FixedHandler(ApprovalDecision::Deny);
        let denied = ensure_tool_call_approved(
            &config,
            &cache,
            "demo",
            &tool(),
            &json!({}),
            ApprovalOrigin::Proxy,
            None,
            Some(&deny),
            no_context,
        );
        assert_eq!(denied, ToolCallApprovalResult::Denied);

        let once = FixedHandler(ApprovalDecision::AllowOnce);
        let allowed = ensure_tool_call_approved(
            &config,
            &cache,
            "demo",
            &tool(),
            &json!({}),
            ApprovalOrigin::Proxy,
            None,
            Some(&once),
            no_context,
        );
        assert_eq!(allowed, ToolCallApprovalResult::Ok);
        assert!(cache.is_empty(), "allow_once is not cached");
    }

    /// Unconfigured servers never prompt, even with a UI handler present.
    #[test]
    fn unconfigured_server_skips_the_ui_handler() {
        let config = config_with_approval(None);
        let cache = ApprovalCache::new();
        let deny = FixedHandler(ApprovalDecision::Deny);
        let result = ensure_tool_call_approved(
            &config,
            &cache,
            "demo",
            &tool(),
            &json!({}),
            ApprovalOrigin::Proxy,
            None,
            Some(&deny),
            no_context,
        );
        assert_eq!(result, ToolCallApprovalResult::Ok);
    }

    /// Broker claims take precedence over the approval requirement and
    /// persist `allow_for_session` like the UI path.
    #[test]
    fn broker_allow_for_session_persists() {
        let config = config_with_approval(None);
        let cache = ApprovalCache::new();
        let sink = Arc::new(RecordingSink::default());
        cache.set_sink(sink.clone());
        let broker = FixedHandler(ApprovalDecision::AllowForSession);
        let result = ensure_tool_call_approved(
            &config,
            &cache,
            "demo",
            &tool(),
            &json!({"query": "x"}),
            ApprovalOrigin::Proxy,
            Some(&broker),
            None,
            no_context,
        );
        assert_eq!(result, ToolCallApprovalResult::Ok);
        assert_eq!(cache.len(), 1);
        assert_eq!(sink.0.lock().unwrap().len(), 1);
    }

    /// A8: rejection details carry names/hashes only, with the proxy `mode`
    /// key first (upstream ordered object).
    #[test]
    fn rejection_details_shape() {
        let (content, details) = approval_rejection_details(
            &ToolCallApprovalResult::ApprovalRequiredHeadless,
            "demo",
            "search",
            Some("call"),
        );
        assert_eq!(
            content,
            json!([{"type": "text", "text": "MCP tool \"search\" on server \"demo\" is approval-gated and requires an interactive session."}])
        );
        assert_eq!(
            details,
            json!({"mode": "call", "error": "approval_required", "server": "demo", "tool": "search"})
        );
        let keys: Vec<&str> = details
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, vec!["mode", "error", "server", "tool"]);

        let (_, direct) =
            approval_rejection_details(&ToolCallApprovalResult::Denied, "demo", "search", None);
        assert_eq!(
            direct,
            json!({"error": "approval_denied", "server": "demo", "tool": "search"})
        );
    }

    /// I-3 (复核): the dialog preview normalizes null/absent args to `{}` and
    /// caps the sanitized text at 500 chars + `...` (tool-approval.ts:160-165
    /// @ 928c30c).
    #[test]
    fn dialog_preview_renders_null_as_empty_object_and_truncates() {
        assert_eq!(dialog_preview(&Value::Null), "{}");
        assert_eq!(dialog_preview(&json!({})), "{}");
        assert_eq!(dialog_preview(&json!({"a": 1})), "{ \"a\": 1 }");
        let long = json!({"q": "x".repeat(600)});
        let preview = dialog_preview(&long);
        assert!(preview.ends_with("..."));
        assert_eq!(preview.chars().count(), 503);
        assert!(!dialog_preview(&json!({"q": "a\u{1b}[31mb\u{7}c"})).contains('\u{1b}'));
    }

    /// Restore clears stale grants before replaying the target branch (A6/A7).
    #[test]
    fn restore_replaces_the_previous_set() {
        let config = config_with_approval(Some(json!(true)));
        let cache = ApprovalCache::new();
        let ui = FixedHandler(ApprovalDecision::AllowForSession);
        let args_a = json!({"query": "a"});
        let identity = get_tool_approval_identity("demo", &tool(), &args_a);
        ensure_tool_call_approved(
            &config,
            &cache,
            "demo",
            &tool(),
            &args_a,
            ApprovalOrigin::Proxy,
            None,
            Some(&ui),
            no_context,
        );
        assert!(cache.is_approved(&identity.cache_key));

        cache.restore(&[]);
        assert!(cache.is_empty());
        assert!(!cache.is_approved(&identity.cache_key));

        let entry = SessionApprovalEntry::allow_for_session(
            "demo",
            "search",
            &identity.definition_hash,
            &identity.args_hash,
        );
        cache.restore(&[entry]);
        assert!(cache.is_approved(&identity.cache_key));
    }
}
