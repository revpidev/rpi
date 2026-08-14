//! fork context: branch the parent session file, filter orchestration
//! entries, align the branch cwd (FR-P0-08).
//!
//! Port of pi-subagents `src/shared/fork-context.ts` @ v0.48.0 (56f97234) plus
//! the message filtering from `subagent-prompt-runtime.ts:59-67,194-209`
//! (upstream filters inside the child's context event; rpi filters
//! file-level at branch time — design §3.4 decision, same resulting child
//! context).
//!
//! Intentional differences: the fork target directory is
//! `<parentSessionDir>/<runId>/run-<index>/` (upstream `createBranchedSession`
//! writes next to the parent; the branch file path is ours to choose since
//! the child only consumes it via `--session`); the thinking-off entry is
//! appended when Anthropic-signed thinking blocks were stripped (upstream
//! `forkedChildRequiresThinkingOff` is conservative — unknown models too —
//! and that conservativeness is kept).

use std::path::{Path, PathBuf};

use serde_json::Value;

/// custom message types dropped from a forked transcript
/// (`PARENT_ONLY_CUSTOM_MESSAGE_TYPES`, subagent-prompt-runtime.ts:59-67).
pub const PARENT_ONLY_CUSTOM_MESSAGE_TYPES: [&str; 8] = [
    "subagent-orchestration-instructions",
    "subagent-slash-result",
    "subagent-slash-text-result",
    "subagent-notify",
    "subagent_control_notice",
    "subagent-control",
    "subagent-control-notice",
    "subagent_watchdog_warning",
];

/// `wrapForkTask` (types.ts:2078-2084).
pub const DEFAULT_FORK_PREAMBLE: &str = "You are a delegated subagent running from a fork of the parent session. Treat the inherited conversation as reference-only context, not a live thread to continue. Do not continue or answer prior messages as if they are waiting for a reply. Your sole job is to execute the task below and return a focused result for that task using your tools.";

pub fn wrap_fork_task(task: &str, preamble: Option<&str>) -> String {
    let effective = preamble.unwrap_or(DEFAULT_FORK_PREAMBLE);
    let prefix = format!("{effective}\n\nTask:\n");
    if task.starts_with(&prefix) {
        task.to_string()
    } else {
        format!("{prefix}{task}")
    }
}

/// `sanitizeUnsafeThinkingBlocks` (fork-context.ts:73-118): drop
/// `redacted_thinking` blocks and Anthropic `thinking` blocks carrying a
/// signature or redaction flag. Returns true when anything was removed.
fn sanitize_unsafe_thinking_blocks(entries: &mut [Value]) -> bool {
    let mut sanitized = false;
    for entry in entries.iter_mut() {
        if entry.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        let Some(message) = entry.get_mut("message").and_then(Value::as_object_mut) else {
            continue;
        };
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let provider = message
            .get("provider")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_lowercase();
        let api = message
            .get("api")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_lowercase();
        let model = message
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_lowercase();
        let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        let before = content.len();
        content.retain(|block| !is_unsafe_anthropic_thinking_block(&provider, &api, &model, block));
        if content.len() != before {
            sanitized = true;
        }
    }
    sanitized
}

fn is_unsafe_anthropic_thinking_block(
    provider: &str,
    api: &str,
    model: &str,
    block: &Value,
) -> bool {
    let Some(block_type) = block.get("type").and_then(Value::as_str) else {
        return false;
    };
    if block_type == "redacted_thinking" {
        return true;
    }
    if block_type != "thinking" {
        return false;
    }
    let is_anthropic =
        provider == "anthropic" || api == "anthropic-messages" || model.starts_with("anthropic/");
    if !is_anthropic {
        return false;
    }
    let signature = block
        .get("thinkingSignature")
        .or_else(|| block.get("signature"))
        .and_then(Value::as_str);
    block.get("redacted") == Some(&Value::Bool(true)) || signature.is_some_and(|s| !s.is_empty())
}

/// Orchestration-entry filter applied to the branched transcript:
/// parent-only custom messages, `subagent` toolResult messages, and
/// `subagent` toolCall blocks inside assistant messages
/// (subagent-prompt-runtime.ts:194-209). Covers both persistence shapes a
/// custom orchestration message can take in rpi session JSONL: top-level
/// `custom`/`custom_message` entries and message entries with `customType`.
pub fn filter_orchestration_entries(entries: &mut Vec<Value>) {
    entries.retain(|entry| {
        let entry_type = entry.get("type").and_then(Value::as_str);
        if matches!(entry_type, Some("custom") | Some("custom_message")) {
            if let Some(custom_type) = entry.get("customType").and_then(Value::as_str) {
                return !PARENT_ONLY_CUSTOM_MESSAGE_TYPES.contains(&custom_type);
            }
            return true;
        }
        if entry_type != Some("message") {
            return true;
        }
        let Some(message) = entry.get("message") else {
            return true;
        };
        if let Some(custom_type) = message.get("customType").and_then(Value::as_str) {
            if PARENT_ONLY_CUSTOM_MESSAGE_TYPES.contains(&custom_type) {
                return false;
            }
        }
        if message.get("role").and_then(Value::as_str) == Some("toolResult")
            && message.get("toolName").and_then(Value::as_str) == Some("subagent")
        {
            return false;
        }
        true
    });
    for entry in entries.iter_mut() {
        let Some(message) = entry.get_mut("message").and_then(Value::as_object_mut) else {
            continue;
        };
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        if let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) {
            let before = content.len();
            content.retain(|block| {
                !(block.get("type").and_then(Value::as_str) == Some("toolCall")
                    && block.get("name").and_then(Value::as_str) == Some("subagent"))
            });
            let _ = before;
        }
    }
}

/// Replay `leaf` entries (rpi session JSONL v3, rpi-agent session.rs:197-209):
/// the last leaf record's targetId (or the entry id itself) wins.
pub fn compute_leaf_id(entries: &[Value]) -> Option<String> {
    let mut leaf: Option<String> = None;
    for entry in entries {
        if entry.get("type").and_then(Value::as_str) == Some("leaf") {
            match entry.get("targetId") {
                Some(Value::String(target)) if !target.is_empty() => {
                    leaf = Some(target.clone());
                }
                _ => {}
            }
        } else if let Some(id) = entry.get("id").and_then(Value::as_str) {
            leaf = Some(id.to_string());
        }
    }
    leaf
}

fn read_session_entries(session_file: &Path) -> Result<Vec<Value>, String> {
    let content = std::fs::read_to_string(session_file).map_err(|error| {
        format!(
            "Unable to inspect forked session {}: {}",
            session_file.to_string_lossy(),
            error
        )
    })?;
    let mut entries = Vec::new();
    for (index, line) in content.split('\n').enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let parsed: Value = serde_json::from_str(line).map_err(|error| {
            format!(
                "Unable to inspect forked session {}: invalid JSONL on line {}: {}",
                session_file.to_string_lossy(),
                index + 1,
                error
            )
        })?;
        entries.push(parsed);
    }
    Ok(entries)
}

/// `alignForkedSessionCwd` (fork-context.ts:132-142): rewrite the session
/// header's `cwd` to the child launch cwd (realpath'd) so the child restores
/// into the right directory.
pub fn align_forked_session_cwd(session_file: &Path, cwd: &Path) -> Result<(), String> {
    let mut entries = read_session_entries(session_file)?;
    let header = entries.first_mut().ok_or_else(|| {
        format!(
            "Forked session {} does not start with a session header.",
            session_file.to_string_lossy()
        )
    })?;
    if header.get("type").and_then(Value::as_str) != Some("session") {
        return Err(format!(
            "Forked session {} does not start with a session header.",
            session_file.to_string_lossy()
        ));
    }
    let resolved = cwd
        .canonicalize()
        .unwrap_or_else(|_| std::path::absolute(cwd).unwrap_or_else(|_| cwd.to_path_buf()));
    let effective = resolved.to_string_lossy().to_string();
    if let Some(map) = header.as_object_mut() {
        map.insert("cwd".into(), Value::String(effective));
    }
    write_session_entries(session_file, &entries)
}

fn write_session_entries(session_file: &Path, entries: &[Value]) -> Result<(), String> {
    let body: Vec<String> = entries
        .iter()
        .map(|entry| serde_json::to_string(entry).unwrap_or_default())
        .collect();
    std::fs::write(session_file, format!("{}\n", body.join("\n"))).map_err(|error| {
        format!(
            "Failed to write forked session {}: {}",
            session_file.to_string_lossy(),
            error
        )
    })
}

/// Entry ids are 8 hex chars (rpi session format); a fresh id avoids
/// colliding with the parent ids.
fn create_entry_id(entries: &[Value]) -> String {
    let existing: std::collections::BTreeSet<&str> = entries
        .iter()
        .filter_map(|entry| entry.get("id").and_then(Value::as_str))
        .collect();
    for _ in 0..100 {
        let id = crate::runner::budget::random_run_id();
        if !existing.contains(id.as_str()) {
            return id;
        }
    }
    crate::runner::budget::random_run_id()
}

fn append_thinking_off_entry(entries: &mut Vec<Value>) {
    if let Some(last) = entries.last() {
        if last.get("type").and_then(Value::as_str) == Some("thinking_level_change")
            && last.get("thinkingLevel").and_then(Value::as_str) == Some("off")
        {
            return;
        }
    }
    let parent = entries
        .iter()
        .rev()
        .find_map(|entry| entry.get("id").and_then(Value::as_str));
    entries.push(serde_json::json!({
        "type": "thinking_level_change",
        "id": create_entry_id(entries),
        "parentId": parent,
        "timestamp": iso_now(),
        "thinkingLevel": "off",
    }));
}

fn iso_now() -> String {
    // RFC3339 with second precision, matching rpi session timestamps.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    crate::artifacts::format_iso8601(now.as_millis() as u64)
}

#[derive(Debug)]
pub struct ForkResolution {
    pub session_file: PathBuf,
    /// Thinking must be forced off for this child (Anthropic signature strip).
    pub thinking_override_off: bool,
}

/// Branch the parent session for a fork-context child
/// (`createForkContextResolver.resolveFork`, fork-context.ts:172-215) with the
/// orchestration filter applied (subagent-prompt-runtime.ts:194-209).
///
/// Fail-fast errors mirror the upstream messages:
/// - no persisted parent session / no leaf / parent file missing
/// - wrapped as `Failed to create forked subagent session: <cause>`.
pub fn create_fork_session(
    parent_session_file: Option<&Path>,
    branch_file: &Path,
    child_cwd: &Path,
) -> Result<ForkResolution, String> {
    let inner = || -> Result<ForkResolution, String> {
        let Some(parent) = parent_session_file else {
            return Err("Forked subagent context requires a persisted parent session.".into());
        };
        let parent_str = parent.to_string_lossy();
        let entries = read_session_entries(parent).map_err(|error| {
            format!(
                "Parent session file does not exist: {parent_str}. rpi has not persisted enough history to fork yet. ({error})"
            )
        })?;
        if entries.is_empty() {
            return Err("Forked subagent context requires a current leaf to fork from.".into());
        }
        if compute_leaf_id(&entries).is_none() {
            return Err("Forked subagent context requires a current leaf to fork from.".into());
        }
        // Branch = copy of the parent entries up to the leaf (the file IS the
        // linear history; the leaf is its last entry), minus orchestration
        // residue, plus the thinking-off marker when signatures were stripped.
        let mut branched = entries;
        filter_orchestration_entries(&mut branched);
        let stripped = sanitize_unsafe_thinking_blocks(&mut branched);
        let mut thinking_off = false;
        if stripped {
            append_thinking_off_entry(&mut branched);
            thinking_off = true;
        }
        if let Some(parent_dir) = branch_file.parent() {
            std::fs::create_dir_all(parent_dir)
                .map_err(|error| format!("Failed to create forked session directory: {error}"))?;
        }
        write_session_entries(branch_file, &branched)?;
        align_forked_session_cwd(branch_file, child_cwd)?;
        Ok(ForkResolution {
            session_file: branch_file.to_path_buf(),
            thinking_override_off: thinking_off,
        })
    };
    inner().map_err(|error| format!("Failed to create forked subagent session: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, kind: &str) -> Value {
        serde_json::json!({"type": kind, "id": id, "timestamp": "2026-08-14T00:00:00Z"})
    }

    #[test]
    fn wrap_fork_task_is_idempotent() {
        let once = wrap_fork_task("do it", None);
        assert!(once.starts_with("You are a delegated subagent"));
        assert!(once.ends_with("Task:\ndo it"));
        assert_eq!(wrap_fork_task(&once, None), once);
    }

    #[test]
    fn leaf_replay_takes_last_target() {
        let entries = vec![
            entry("aaaa1111", "message"),
            serde_json::json!({"type": "leaf", "id": "bbbb2222", "targetId": "cccc3333"}),
        ];
        assert_eq!(compute_leaf_id(&entries).as_deref(), Some("cccc3333"));
    }

    #[test]
    fn orchestration_filter_drops_subagent_traces() {
        let mut entries = vec![
            serde_json::json!({"type":"message","id":"1","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}),
            serde_json::json!({"type":"message","id":"2","message":{"role":"assistant","content":[{"type":"toolCall","id":"t1","name":"subagent","arguments":{}},{"type":"text","text":"delegate"}]}}),
            serde_json::json!({"type":"message","id":"3","message":{"role":"toolResult","toolName":"subagent","content":[]}}),
            serde_json::json!({"type":"message","id":"4","message":{"role":"user","customType":"subagent-control","content":[]}}),
            serde_json::json!({"type":"message","id":"5","message":{"role":"user","content":[{"type":"text","text":"keep"}]}}),
        ];
        filter_orchestration_entries(&mut entries);
        assert_eq!(entries.len(), 3);
        let assistant = entries[1].get("message").unwrap();
        assert_eq!(
            assistant.get("content").unwrap().as_array().unwrap().len(),
            1,
            "subagent toolCall block removed"
        );
    }

    #[test]
    fn anthropic_thinking_blocks_are_stripped() {
        let mut entries = vec![serde_json::json!({
            "type": "message", "id": "1",
            "message": {"role": "assistant", "provider": "anthropic", "content": [
                {"type": "thinking", "thinking": "h", "signature": "sig123"},
                {"type": "thinking", "thinking": "h"},
                {"type": "text", "text": "answer"}
            ]}
        })];
        let stripped = sanitize_unsafe_thinking_blocks(&mut entries);
        assert!(stripped);
        let content = entries[0]
            .get("message")
            .unwrap()
            .get("content")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(content.len(), 2);
        // Unsigned thinking blocks stay.
        assert_eq!(content[0].get("type").unwrap(), "thinking");
    }

    #[test]
    fn fork_fail_fast_without_parent() {
        let err =
            create_fork_session(None, Path::new("/tmp/x.jsonl"), Path::new("/tmp")).unwrap_err();
        assert!(err.contains("requires a persisted parent session"), "{err}");
    }
}
