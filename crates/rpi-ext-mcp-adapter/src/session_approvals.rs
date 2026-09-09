//! Session-scoped MCP tool approvals: identity hashes, the persisted
//! `mcp-approval-v1` custom entry, and active-branch replay.
//!
//! Port of `session-approvals.ts` @ `928c30c` (#505/#492, the commit that
//! implements the persistence requirement R7.2.2.3; the file is not present
//! at the pinned v2.32.1 tag `10a45367`) plus the `getToolApprovalIdentity`
//! consumer in `tool-approval.ts` @ `928c30c`.
//!
//! rpi has no `ctx.sessionManager.getBranch()` host-call (04-design §6.1), so
//! the read side is the documented transitional path: `ctx.sessionFile.path`
//! straight JSONL read, replayed per `customType` (R7.2.2.3). The active
//! branch is walked from the leaf id the host reports on `session_tree`
//! (agent-session.ts:3288-3295 @ 9841914), or from the last entry on
//! session load (the host's own `build_index` leaf semantics).
//!
//! Security red line (G4): entries carry only names and SHA-256 hashes; raw
//! arguments are never serialized into a session entry.

use std::collections::HashSet;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::cache::stable_stringify;
use crate::metadata::ToolMetadata;

/// `MCP_APPROVAL_CUSTOM_TYPE` (session-approvals.ts:6 @ 928c30c).
pub const MCP_APPROVAL_CUSTOM_TYPE: &str = "mcp-approval-v1";

/// `SessionApprovalEntry` (session-approvals.ts:8-25 @ 928c30c), tool grant
/// variant. The iframe-consent variant is P2 (MCP UI) and never persisted by
/// this port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionApprovalEntry {
    pub version: u8,
    pub kind: String,
    pub decision: String,
    pub server_name: String,
    pub original_tool_name: String,
    pub definition_hash: String,
    pub args_hash: String,
}

impl SessionApprovalEntry {
    /// The only tool-grant record upstream writes
    /// (`decision: "allow_for_session"`, session-approvals.ts:161-169).
    pub fn allow_for_session(
        server_name: &str,
        original_tool_name: &str,
        definition_hash: &str,
        args_hash: &str,
    ) -> Self {
        Self {
            version: 1,
            kind: "tool".to_string(),
            decision: "allow_for_session".to_string(),
            server_name: server_name.to_string(),
            original_tool_name: original_tool_name.to_string(),
            definition_hash: definition_hash.to_string(),
            args_hash: args_hash.to_string(),
        }
    }
}

/// `TOOL_APPROVAL_KEYS` (session-approvals.ts:31-39 @ 928c30c): the exact
/// key set a strict tool entry may carry.
const TOOL_APPROVAL_KEYS: [&str; 7] = [
    "version",
    "kind",
    "decision",
    "serverName",
    "originalToolName",
    "definitionHash",
    "argsHash",
];

fn is_non_empty_string(value: Option<&Value>) -> bool {
    value.and_then(Value::as_str).is_some_and(|s| !s.is_empty())
}

/// `SHA256_HEX` (session-approvals.ts:41 @ 928c30c): 64 lowercase hex chars.
fn is_sha256(value: Option<&Value>) -> bool {
    value.and_then(Value::as_str).is_some_and(|s| {
        s.len() == 64
            && s.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    })
}

/// `isSessionApprovalEntry` (session-approvals.ts:114-130 @ 928c30c):
/// strict exact-key/type validation. Unknown or malformed payloads are
/// rejected without panicking (task §4.2 A9).
pub fn is_session_approval_entry(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    if object.get("version").and_then(Value::as_u64) != Some(1) {
        return false;
    }
    if !is_non_empty_string(object.get("serverName")) {
        return false;
    }
    if object.get("kind").and_then(Value::as_str) != Some("tool") {
        return false;
    }
    if object.len() != TOOL_APPROVAL_KEYS.len()
        || !TOOL_APPROVAL_KEYS
            .iter()
            .all(|key| object.contains_key(*key))
    {
        return false;
    }
    object.get("decision").and_then(Value::as_str) == Some("allow_for_session")
        && is_non_empty_string(object.get("originalToolName"))
        && is_sha256(object.get("definitionHash"))
        && is_sha256(object.get("argsHash"))
}

/// Parse a strict tool-grant entry, `None` for anything else.
pub fn parse_session_approval_entry(value: &Value) -> Option<SessionApprovalEntry> {
    if !is_session_approval_entry(value) {
        return None;
    }
    serde_json::from_value(value.clone()).ok()
}

/// `stableStringify` + `computeToolArgumentsHash` (session-approvals.ts:43-52
/// @ 928c30c). `args ?? {}` upstream: a null/absent payload hashes as the
/// empty object.
pub fn compute_tool_arguments_hash(args: &Value) -> String {
    let normalized = if args.is_null() {
        json!({})
    } else {
        args.clone()
    };
    sha256_hex(&stable_stringify(&normalized))
}

/// `computeToolDefinitionHash` (session-approvals.ts:54-63 @ 928c30c): the
/// effective tool definition — original name, input schema and the resource /
/// UI resource URIs (rpi has no `uiResourceUri`; P2, so the key is always
/// `null`, matching upstream's `?? null`).
pub fn compute_tool_definition_hash(tool: &ToolMetadata) -> String {
    let definition = json!({
        "originalName": tool.original_name,
        "inputSchema": tool.input_schema.clone().unwrap_or(Value::Null),
        "resourceUri": tool.resource_uri.clone().map(Value::String).unwrap_or(Value::Null),
        "uiResourceUri": Value::Null,
    });
    sha256_hex(&stable_stringify(&definition))
}

/// `makeToolApprovalKey` (session-approvals.ts:65-72 @ 928c30c): four-part
/// cache key `server\0originalTool\0definitionHash\0argsHash`.
pub fn make_tool_approval_key(
    server_name: &str,
    original_tool_name: &str,
    definition_hash: &str,
    args_hash: &str,
) -> String {
    format!("{server_name}\u{0}{original_tool_name}\u{0}{definition_hash}\u{0}{args_hash}")
}

/// `getToolApprovalIdentity` (session-approvals.ts:74-87 @ 928c30c).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolApprovalIdentity {
    pub definition_hash: String,
    pub args_hash: String,
    pub cache_key: String,
}

pub fn get_tool_approval_identity(
    server_name: &str,
    tool: &ToolMetadata,
    args: &Value,
) -> ToolApprovalIdentity {
    let definition_hash = compute_tool_definition_hash(tool);
    let args_hash = compute_tool_arguments_hash(args);
    ToolApprovalIdentity {
        cache_key: make_tool_approval_key(
            server_name,
            &tool.original_name,
            &definition_hash,
            &args_hash,
        ),
        definition_hash,
        args_hash,
    }
}

/// `SessionApprovalWriter` (session-approvals.ts:89 @ 928c30c): the host
/// `appendEntry` sink. Implementations must be fail-soft (upstream logs at
/// debug and never propagates).
pub trait SessionApprovalSink: Send + Sync {
    fn append(&self, entry: &SessionApprovalEntry);
}

/// `restoreSessionApprovalState` (session-approvals.ts:178-199 @ 928c30c):
/// idempotent rebuild — clear then replay, no incremental patching.
pub fn restored_approval_keys(entries: &[SessionApprovalEntry]) -> HashSet<String> {
    entries
        .iter()
        .map(|entry| {
            make_tool_approval_key(
                &entry.server_name,
                &entry.original_tool_name,
                &entry.definition_hash,
                &entry.args_hash,
            )
        })
        .collect()
}

/// Parse JSONL content into raw entry objects, skipping the session header,
/// blank lines and malformed lines (task §4.2 A9 — corrupt entries are
/// dropped without losing the valid ones).
pub fn parse_session_entry_lines(content: &str) -> Vec<Value> {
    let mut entries = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            tracing::debug!("MCP: skipping malformed session line");
            continue;
        };
        if value.get("type").and_then(Value::as_str) == Some("session") {
            continue;
        }
        if value.get("id").and_then(Value::as_str).is_none() {
            continue;
        }
        entries.push(value);
    }
    entries
}

/// Active branch from a parsed entry list (host `buildSessionPath` +
/// `build_index` leaf semantics): an explicit `leaf_id` wins (unknown id →
/// empty branch); otherwise the last entry is the leaf, with harness `leaf`
/// records moving the pointer to their `targetId`.
pub fn active_branch<'a>(entries: &'a [Value], leaf_id: Option<&str>) -> Vec<&'a Value> {
    let by_id: std::collections::HashMap<&str, &Value> = entries
        .iter()
        .filter_map(|entry| Some((entry.get("id")?.as_str()?, entry)))
        .collect();

    let leaf: Option<&Value> = match leaf_id {
        Some(id) => by_id.get(id).copied(),
        None => {
            let mut pointer: Option<&str> = None;
            for entry in entries {
                if entry.get("type").and_then(Value::as_str) == Some("leaf") {
                    pointer = entry.get("targetId").and_then(Value::as_str);
                } else if let Some(id) = entry.get("id").and_then(Value::as_str) {
                    pointer = Some(id);
                }
            }
            pointer.and_then(|id| by_id.get(id).copied())
        }
    };

    let Some(mut current) = leaf else {
        return Vec::new();
    };
    let mut path = Vec::new();
    loop {
        path.push(current);
        current = match current
            .get("parentId")
            .and_then(Value::as_str)
            .and_then(|parent| by_id.get(parent).copied())
        {
            Some(parent) => parent,
            None => break,
        };
    }
    path.reverse();
    path
}

/// Replay the `mcp-approval-v1` tool grants from a branch (root → leaf
/// order). Unknown `customType`s and invalid payloads are skipped.
pub fn approval_entries_from_branch(branch: &[&Value]) -> Vec<SessionApprovalEntry> {
    let mut entries = Vec::new();
    for entry in branch {
        if entry.get("type").and_then(Value::as_str) != Some("custom")
            || entry.get("customType").and_then(Value::as_str) != Some(MCP_APPROVAL_CUSTOM_TYPE)
        {
            continue;
        }
        let Some(data) = entry.get("data") else {
            continue;
        };
        match parse_session_approval_entry(data) {
            Some(parsed) => entries.push(parsed),
            None => tracing::debug!("MCP: skipping unrecognized session approval entry"),
        }
    }
    entries
}

/// Read + replay the approvals of one session file's active branch.
pub fn read_session_approval_entries(
    path: &Path,
    leaf_id: Option<&str>,
) -> std::io::Result<Vec<SessionApprovalEntry>> {
    let content = std::fs::read_to_string(path)?;
    let entries = parse_session_entry_lines(&content);
    let branch = active_branch(&entries, leaf_id);
    Ok(approval_entries_from_branch(&branch))
}

fn sha256_hex(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    let mut hex = String::with_capacity(64);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// Serialize an entry to the `appendEntry` payload shape (camelCase keys,
/// exact upstream key order).
pub fn entry_to_value(entry: &SessionApprovalEntry) -> Value {
    let mut object = Map::new();
    object.insert("version".to_string(), json!(entry.version));
    object.insert("kind".to_string(), json!(entry.kind));
    object.insert("decision".to_string(), json!(entry.decision));
    object.insert("serverName".to_string(), json!(entry.server_name));
    object.insert(
        "originalToolName".to_string(),
        json!(entry.original_tool_name),
    );
    object.insert("definitionHash".to_string(), json!(entry.definition_hash));
    object.insert("argsHash".to_string(), json!(entry.args_hash));
    Value::Object(object)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(original: &str, schema: Value) -> ToolMetadata {
        ToolMetadata {
            name: format!("demo_{original}"),
            original_name: original.to_string(),
            description: String::new(),
            resource_uri: None,
            input_schema: Some(schema),
        }
    }

    /// A3: key order and nesting do not change the arguments hash.
    #[test]
    fn arguments_hash_is_key_order_independent_and_stable() {
        let a = json!({"query": "demo", "options": {"limit": 5, "reverse": true}});
        let b = json!({"options": {"reverse": true, "limit": 5}, "query": "demo"});
        assert_eq!(
            compute_tool_arguments_hash(&a),
            compute_tool_arguments_hash(&b)
        );
        assert_ne!(
            compute_tool_arguments_hash(&a),
            compute_tool_arguments_hash(&json!({"query": "other"}))
        );
        // A4: nested arrays + Unicode are stable (same UTF-16 key sort as
        // `stable_stringify`, Unicode preserved).
        let nested = json!({"z": [1, {"b": "ü", "a": ["x", "é"]}], "a": null});
        assert_eq!(
            compute_tool_arguments_hash(&nested),
            compute_tool_arguments_hash(&nested.clone())
        );
        // `args ?? {}`: null hashes as the empty object.
        assert_eq!(
            compute_tool_arguments_hash(&Value::Null),
            compute_tool_arguments_hash(&json!({}))
        );
    }

    /// A4: definition hash ignores key order, tracks schema/resource changes.
    #[test]
    fn definition_hash_is_deterministic_and_definition_sensitive() {
        let same = tool(
            "search",
            json!({"properties": {"query": {"type": "string"}}, "type": "object"}),
        );
        let reordered = tool(
            "search",
            json!({"type": "object", "properties": {"query": {"type": "string"}}}),
        );
        assert_eq!(
            compute_tool_definition_hash(&same),
            compute_tool_definition_hash(&reordered)
        );
        let changed = tool(
            "search",
            json!({"type": "object", "properties": {"query": {"type": "number"}}}),
        );
        assert_ne!(
            compute_tool_definition_hash(&same),
            compute_tool_definition_hash(&changed)
        );
        let mut resource = same.clone();
        resource.resource_uri = Some("mcp://demo/search".to_string());
        assert_ne!(
            compute_tool_definition_hash(&same),
            compute_tool_definition_hash(&resource)
        );
    }

    /// A1/A2 foundation: distinct args ⇒ distinct cache keys; same args ⇒
    /// same key.
    #[test]
    fn cache_key_scopes_to_definition_and_arguments() {
        let tool = tool("search", json!({"type": "object"}));
        let a = get_tool_approval_identity("demo", &tool, &json!({"query": "a"}));
        let b = get_tool_approval_identity("demo", &tool, &json!({"query": "b"}));
        let a2 = get_tool_approval_identity("demo", &tool, &json!({"query": "a"}));
        assert_ne!(a.cache_key, b.cache_key);
        assert_eq!(a.cache_key, a2.cache_key);
        assert_eq!(
            a.cache_key,
            make_tool_approval_key("demo", "search", &a.definition_hash, &a.args_hash)
        );
        assert_eq!(a.cache_key.split('\u{0}').count(), 4, "four-part key");
    }

    /// A8/A9: strict exact-key parsing, no raw-argument field can pass.
    #[test]
    fn entry_validation_is_strict() {
        let identity = get_tool_approval_identity(
            "demo",
            &tool("search", json!({"type": "object"})),
            &json!({"query": "x"}),
        );
        let entry = SessionApprovalEntry::allow_for_session(
            "demo",
            "search",
            &identity.definition_hash,
            &identity.args_hash,
        );
        let value = entry_to_value(&entry);
        assert!(is_session_approval_entry(&value));
        assert_eq!(parse_session_approval_entry(&value), Some(entry.clone()));

        // Extra key (e.g. smuggled raw args) rejects the whole record.
        let mut extra = value.clone();
        extra["args"] = json!({"query": "x"});
        assert!(!is_session_approval_entry(&extra));
        // Missing key rejects.
        let mut missing = value.clone();
        missing.as_object_mut().expect("object").remove("argsHash");
        assert!(!is_session_approval_entry(&missing));
        // Bad hash shape rejects.
        let mut bad_hash = value.clone();
        bad_hash["argsHash"] = json!("not-a-sha");
        assert!(!is_session_approval_entry(&bad_hash));
        // Uppercase hex rejects (upstream `SHA256_HEX` is lowercase-only).
        let mut upper = value.clone();
        upper["definitionHash"] = json!(identity.definition_hash.to_uppercase());
        assert!(!is_session_approval_entry(&upper));
        // Wrong kind / decision rejects.
        let mut wrong_kind = value.clone();
        wrong_kind["kind"] = json!("iframe");
        assert!(!is_session_approval_entry(&wrong_kind));
        // Non-object rejects without panicking.
        assert!(!is_session_approval_entry(&json!("nope")));
    }

    fn custom_entry(id: &str, parent: Option<&str>, entry: &SessionApprovalEntry) -> Value {
        json!({
            "type": "custom",
            "customType": MCP_APPROVAL_CUSTOM_TYPE,
            "data": entry_to_value(entry),
            "id": id,
            "parentId": parent,
            "timestamp": "2026-09-09T00:00:00.000Z",
        })
    }

    /// A6/A7 foundation: branch walk from an explicit leaf and from the file
    /// tip, with unrelated branches excluded.
    #[test]
    fn branch_walk_selects_the_target_branch() {
        let identity_a = get_tool_approval_identity(
            "demo",
            &tool("search", json!({"type": "object"})),
            &json!({"query": "a"}),
        );
        let identity_b = get_tool_approval_identity(
            "demo",
            &tool("search", json!({"type": "object"})),
            &json!({"query": "b"}),
        );
        let entry_a = SessionApprovalEntry::allow_for_session(
            "demo",
            "search",
            &identity_a.definition_hash,
            &identity_a.args_hash,
        );
        let entry_b = SessionApprovalEntry::allow_for_session(
            "demo",
            "search",
            &identity_b.definition_hash,
            &identity_b.args_hash,
        );

        let entries = vec![
            custom_entry("e1", None, &entry_a),
            custom_entry("e2", Some("e1"), &entry_b),
        ];

        // Explicit leaf on the e2 branch → both grants.
        let branch = active_branch(&entries, Some("e2"));
        assert_eq!(approval_entries_from_branch(&branch).len(), 2);
        // Explicit leaf e1 → only the first grant.
        let branch = active_branch(&entries, Some("e1"));
        assert_eq!(approval_entries_from_branch(&branch), vec![entry_a.clone()]);
        // Unknown leaf → empty (host `getBranch(unknown) → []`).
        assert!(active_branch(&entries, Some("missing")).is_empty());
        // No leaf → last entry (host `buildSessionPath` default).
        assert_eq!(
            approval_entries_from_branch(&active_branch(&entries, None)),
            vec![entry_a.clone(), entry_b.clone()]
        );

        // Divergent branch: e3 (child of e1) is the tip with a third grant,
        // so the e2 grant is not on the active branch.
        let identity_c = get_tool_approval_identity(
            "demo",
            &tool("search", json!({"type": "object"})),
            &json!({"query": "c"}),
        );
        let entry_c = SessionApprovalEntry::allow_for_session(
            "demo",
            "search",
            &identity_c.definition_hash,
            &identity_c.args_hash,
        );
        let mut diverged = entries.clone();
        diverged.push(custom_entry("e3", Some("e1"), &entry_c));
        let branch = active_branch(&diverged, None);
        assert_eq!(
            approval_entries_from_branch(&branch),
            vec![entry_a.clone(), entry_c.clone()]
        );
        let branch = active_branch(&diverged, Some("e2"));
        assert_eq!(
            approval_entries_from_branch(&branch),
            vec![entry_a.clone(), entry_b.clone()]
        );
        let branch = active_branch(&diverged, Some("e1"));
        assert_eq!(approval_entries_from_branch(&branch), vec![entry_a]);
    }

    /// A9: malformed lines and unknown entries are skipped without losing
    /// valid entries.
    #[test]
    fn corrupt_lines_and_unknown_entries_are_skipped() {
        let identity = get_tool_approval_identity(
            "demo",
            &tool("search", json!({"type": "object"})),
            &json!({"query": "a"}),
        );
        let entry = SessionApprovalEntry::allow_for_session(
            "demo",
            "search",
            &identity.definition_hash,
            &identity.args_hash,
        );
        let valid = serde_json::to_string(&custom_entry("e2", Some("e1"), &entry)).expect("json");
        let content = format!(
            "{}\n{{not json}}\n{}\n{}\n",
            json!({"type": "session", "id": "s1"}),
            json!({"type": "custom", "customType": "other", "id": "e1", "parentId": null}),
            valid
        );
        let entries = parse_session_entry_lines(&content);
        let branch = active_branch(&entries, None);
        assert_eq!(approval_entries_from_branch(&branch), vec![entry]);
    }

    /// A6/A7: restore is a clear-and-replay rebuild; duplicates collapse.
    #[test]
    fn restored_keys_are_idempotent() {
        let identity = get_tool_approval_identity(
            "demo",
            &tool("search", json!({"type": "object"})),
            &json!({"query": "a"}),
        );
        let entry = SessionApprovalEntry::allow_for_session(
            "demo",
            "search",
            &identity.definition_hash,
            &identity.args_hash,
        );
        let keys = restored_approval_keys(&[entry.clone(), entry.clone()]);
        assert_eq!(keys.len(), 1);
        assert!(keys.contains(&identity.cache_key));
    }
}
