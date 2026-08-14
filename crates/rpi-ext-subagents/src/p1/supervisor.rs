//! Supervisor coordination channel (FR-P1-10): child-facing
//! `contact_supervisor` and parent-facing `subagent_supervisor` over a
//! per-child file channel.
//!
//! Port of pi-subagents `src/intercom/native-supervisor-channel.ts` @ v0.48.0
//! (56f97234) + `intercom-bridge.ts`: requests carry the orchestrator session
//! id and only the owning session sees them; `progress_update` writes and
//! returns; other reasons poll `replies/<id>.json` (≤500ms interval, 10min
//! default timeout); `interview_request` parses fenced JSON replies. The
//! bridge (`intercomBridge` mode always|fork-only|off) adds
//! `contact_supervisor` to agent tools and appends the instruction block.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

pub const SUPERVISOR_CHANNEL_DIR_ENV: &str = "RPI_SUBAGENT_SUPERVISOR_CHANNEL_DIR";
pub const SUPERVISOR_CHILD_INDEX_ENV: &str = "RPI_SUBAGENT_CHILD_INDEX";
/// Default blocking wait for a supervisor reply (`DEFAULT_ASK_TIMEOUT_MS`,
/// native-supervisor-channel.ts:23).
pub const DEFAULT_ASK_TIMEOUT_MS: u64 = 10 * 60 * 1000;
/// Requests are capped at 64 KiB (L262).
pub const MAX_REQUEST_BYTES: usize = 64 * 1024;

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn sanitize_target_part(value: &str) -> String {
    let cleaned: String = value
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('-');
    if trimmed.is_empty() {
        "agent".to_string()
    } else {
        trimmed.to_string()
    }
}

/// `resolveSupervisorChannelDir` (L97): `<tempRoot>/supervisor-channels/
/// <safeRunId>-<safeAgent>-<childIndex>/{requests,replies}` (0o700).
pub fn channel_dir(run_id: &str, agent: &str, child_index: usize) -> PathBuf {
    crate::paths::temp_root_dir()
        .join("supervisor-channels")
        .join(format!(
            "{}-{}-{}",
            sanitize_target_part(run_id),
            sanitize_target_part(agent),
            child_index
        ))
}

pub fn ensure_channel(dir: &Path) {
    let _ = std::fs::create_dir_all(dir.join("requests"));
    let _ = std::fs::create_dir_all(dir.join("replies"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
}

/// `formatChildMessage` header (L151): reason title + run context lines.
pub fn format_child_message(reason: &str, run_id: &str, agent: &str, child_index: usize, message: &str) -> String {
    let title = match reason {
        "need_decision" => "Subagent needs a decision",
        "interview_request" => "Subagent requests structured input",
        _ => "Subagent progress update",
    };
    format!(
        "{title}\nRun: {run_id}\nAgent: {agent}\nChild index: {child_index}\n\n{message}"
    )
}

/// One pending request projected for the parent listing.
pub fn read_requests(dir: &Path) -> Vec<Value> {
    let mut requests = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir.join("requests")) else {
        return requests;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if let Ok(raw) = std::fs::read_to_string(&path) {
            if let Ok(value) = serde_json::from_str::<Value>(&raw) {
                requests.push(value);
            }
        }
    }
    requests.sort_by(|a, b| {
        a["createdAt"]
            .as_str()
            .cmp(&b["createdAt"].as_str())
    });
    requests
}

/// `requestMatchesContext` (L453): only the orchestrator session sees the
/// request (wrong-session lifecycle).
pub fn request_matches_session(request: &Value, orchestrator_session_id: &str) -> bool {
    request["orchestratorSessionId"].as_str() == Some(orchestrator_session_id)
}

/// Child-side `contact_supervisor` execution (registerNativeSupervisorClient
/// L298-280 subset). Blocking reply wait runs on the plugin runtime.
pub struct ChildSupervisorContext {
    pub channel_dir: PathBuf,
    pub run_id: String,
    pub agent: String,
    pub child_index: usize,
    pub orchestrator_session_id: String,
}

impl ChildSupervisorContext {
    pub fn from_env() -> Option<Self> {
        let channel_dir = std::env::var(SUPERVISOR_CHANNEL_DIR_ENV).ok()?;
        let run_id = std::env::var(crate::launch::args::SUBAGENT_RUN_ID_ENV).ok()?;
        let agent = std::env::var(crate::launch::args::SUBAGENT_CHILD_AGENT_ENV).ok()?;
        let child_index = std::env::var(SUPERVISOR_CHILD_INDEX_ENV)
            .ok()
            .and_then(|raw| raw.parse::<usize>().ok())
            .unwrap_or(0);
        let orchestrator_session_id = std::env::var(crate::launch::args::SUBAGENT_ORCHESTRATOR_SESSION_ID_ENV)
            .ok()
            .unwrap_or_default();
        Some(Self {
            channel_dir: PathBuf::from(channel_dir),
            run_id,
            agent,
            child_index,
            orchestrator_session_id,
        })
    }

    pub fn tool_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "reason": {
                    "type": "string",
                    "enum": ["need_decision", "interview_request", "progress_update"],
                    "description": "Why the parent is being contacted: need_decision (blocking clarification), interview_request (structured input), progress_update (non-blocking note)."
                },
                "message": { "type": "string", "description": "The question or update text." },
                "interview": { "type": "object", "description": "Structured interview fields for interview_request." }
            },
            "required": ["reason"]
        })
    }

    pub fn execute(&self, params: &Value) -> Value {
        let reason = params
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("progress_update");
        if !matches!(
            reason,
            "need_decision" | "interview_request" | "progress_update"
        ) {
            return error_result("reason must be need_decision, interview_request, or progress_update.");
        }
        let message = params
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let formatted = format_child_message(reason, &self.run_id, &self.agent, self.child_index, message);
        let request_id = crate::runner::budget::random_run_id();
        let request = json!({
            "type": "subagent.supervisor.request",
            "id": request_id,
            "createdAt": crate::artifacts::format_iso8601(now_millis()),
            "reason": reason,
            "message": formatted,
            "expectsReply": reason != "progress_update",
            "orchestratorSessionId": self.orchestrator_session_id,
            "runId": self.run_id,
            "agent": self.agent,
            "childIndex": self.child_index,
            "interview": params.get("interview").cloned().unwrap_or(Value::Null),
        });
        let serialized = request.to_string();
        if serialized.len() > MAX_REQUEST_BYTES {
            return error_result("supervisor request exceeds the 64 KiB channel limit; shorten the message.");
        }
        ensure_channel(&self.channel_dir);
        let request_path = self.channel_dir.join("requests").join(format!("{request_id}.json"));
        if let Err(error) = std::fs::write(&request_path, serialized) {
            return error_result(&format!("failed to write the supervisor request: {error}"));
        }
        // progress_update writes and returns immediately (L265-270).
        if reason == "progress_update" {
            return ok_result("Progress update delivered.");
        }
        // Poll the reply inbox (≤500ms interval, 10min timeout, L272-279).
        let reply_path = self.channel_dir.join("replies").join(format!("{request_id}.json"));
        let deadline = Instant::now() + Duration::from_millis(DEFAULT_ASK_TIMEOUT_MS);
        loop {
            if let Ok(raw) = std::fs::read_to_string(&reply_path) {
                let _ = std::fs::remove_file(&reply_path);
                let reply: Value = serde_json::from_str(&raw).unwrap_or(json!({}));
                let message = reply
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if reason == "interview_request" {
                    // Fenced JSON extraction (L179-189).
                    if let Some(start) = message.find("```") {
                        if let Some(body) = message[start + 3..].strip_prefix("json\n").or_else(|| {
                            message[start + 3..].split_once('\n').map(|(_, rest)| rest)
                        }) {
                            if let Some(end) = body.find("```") {
                                if let Ok(parsed) = serde_json::from_str::<Value>(body[..end].trim()) {
                                    return json!({
                                        "content": [{ "type": "text", "text": message }],
                                        "details": { "structured": parsed },
                                        "isError": false,
                                    });
                                }
                            }
                        }
                    }
                }
                return ok_result(&message);
            }
            if Instant::now() >= deadline {
                return error_result("Timed out waiting for the supervisor's reply.");
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }
}

fn ok_result(text: &str) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": false,
    })
}

fn error_result(text: &str) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": true,
    })
}

/// Parent-side `subagent_supervisor` ({action: pending|reply}) —
/// `NATIVE_SUPERVISOR_TOOL_NAME` handler (L559-587).
pub fn parent_supervisor_action(
    action: &str,
    reply_to: Option<&str>,
    message: Option<&str>,
    orchestrator_session_id: &str,
    channels_root: &Path,
) -> Value {
    match action {
        "pending" | "list" => {
            let mut lines = vec!["Pending supervisor requests:".to_string()];
            let mut found = 0;
            if let Ok(channel_entries) = std::fs::read_dir(channels_root) {
                for channel in channel_entries.flatten() {
                    for request in read_requests(&channel.path()) {
                        if !request_matches_session(&request, orchestrator_session_id) {
                            continue;
                        }
                        found += 1;
                        lines.push(format!(
                            "- id={} reason={} run={} agent={} child={}",
                            request["id"].as_str().unwrap_or("?"),
                            request["reason"].as_str().unwrap_or("?"),
                            request["runId"].as_str().unwrap_or("?"),
                            request["agent"].as_str().unwrap_or("?"),
                            request["childIndex"].as_u64().unwrap_or(0),
                        ));
                        if let Some(text) = request["message"].as_str() {
                            lines.push(format!("  {text}"));
                        }
                    }
                }
            }
            if found == 0 {
                lines.push("- (none)".to_string());
            }
            json!({
                "content": [{ "type": "text", "text": lines.join("\n") }],
                "isError": false,
            })
        }
        "reply" => {
            let (Some(reply_to), Some(message)) = (reply_to, message) else {
                return error_result("action \"reply\" requires replyTo and message.");
            };
            // Find the matching request across channels (wrong sessions never
            // see it, so the id alone is the key).
            if let Ok(channel_entries) = std::fs::read_dir(channels_root) {
                for channel in channel_entries.flatten() {
                    let request_path = channel.path().join("requests").join(format!("{reply_to}.json"));
                    if !request_path.exists() {
                        continue;
                    }
                    let reply = json!({
                        "type": "subagent.supervisor.reply",
                        "requestId": reply_to,
                        "createdAt": crate::artifacts::format_iso8601(now_millis()),
                        "message": message,
                    });
                    let reply_path = channel
                        .path()
                        .join("replies")
                        .join(format!("{reply_to}.json"));
                    if let Err(error) = std::fs::write(&reply_path, reply.to_string()) {
                        return error_result(&format!("failed to write the reply: {error}"));
                    }
                    let _ = std::fs::remove_file(&request_path);
                    return ok_result(&format!("Replied to supervisor request {reply_to}."));
                }
            }
            error_result(&format!("No pending supervisor request matches '{reply_to}'."))
        }
        other => error_result(&format!(
            "Unknown subagent_supervisor action \"{other}\". Supported: pending, reply."
        )),
    }
}

/// `applyIntercomBridgeToAgent` (intercom-bridge.ts:169): active bridges add
/// `contact_supervisor` to the agent tool list and append the instruction
/// block to the system prompt.
pub fn apply_intercom_bridge(
    mode: &str,
    context: Option<&str>,
    tools: &mut Option<Vec<String>>,
    system_prompt: &mut String,
) {
    let active = match mode {
        "off" => false,
        "fork-only" => context == Some("fork"),
        _ => true, // always (default)
    };
    if !active {
        return;
    }
    if let Some(tool_list) = tools.as_mut() {
        if !tool_list.iter().any(|t| t == "contact_supervisor") {
            tool_list.push("contact_supervisor".to_string());
        }
    }
    let instruction = "Intercom orchestration channel: contact_supervisor reaches the parent orchestrator (this session's owner). Use reason need_decision for blocking decisions or clarifications, interview_request for structured input, progress_update for short non-blocking updates when a discovery changes the plan. Do not send routine completion handoffs.";
    if !system_prompt.contains("Intercom orchestration channel:") {
        if system_prompt.is_empty() {
            *system_prompt = instruction.to_string();
        } else {
            system_prompt.push_str("\n\n");
            system_prompt.push_str(instruction);
        }
    }
}

/// Channels root for the parent tool.
pub fn channels_root() -> PathBuf {
    crate::paths::temp_root_dir().join("supervisor-channels")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_message_format() {
        let text = format_child_message("need_decision", "run1", "worker", 2, "Ship or hold?");
        assert!(text.starts_with("Subagent needs a decision"));
        assert!(text.contains("Run: run1"));
        assert!(text.contains("Agent: worker"));
        assert!(text.contains("Child index: 2"));
    }

    #[test]
    fn bridge_modes() {
        let mut tools = Some(vec!["read".to_string()]);
        let mut prompt = String::new();
        apply_intercom_bridge("off", Some("fork"), &mut tools, &mut prompt);
        assert_eq!(tools.as_ref().unwrap().len(), 1);
        assert!(prompt.is_empty());

        apply_intercom_bridge("fork-only", Some("fresh"), &mut tools, &mut prompt);
        assert_eq!(tools.as_ref().unwrap().len(), 1);

        apply_intercom_bridge("fork-only", Some("fork"), &mut tools, &mut prompt);
        assert!(tools.as_ref().unwrap().contains(&"contact_supervisor".to_string()));
        assert!(prompt.contains("Intercom orchestration channel:"));

        apply_intercom_bridge("always", None, &mut tools, &mut prompt);
        // Idempotent: no duplicate tool or prompt block.
        assert_eq!(
            tools.as_ref().unwrap().iter().filter(|t| *t == "contact_supervisor").count(),
            1
        );
        assert_eq!(prompt.matches("Intercom orchestration channel:").count(), 1);
    }

    #[test]
    fn request_reply_roundtrip_and_session_routing() {
        let root = channels_root().join(format!("test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let channel = root.join("runx-worker-0");
        ensure_channel(&channel);
        let request = json!({
            "type": "subagent.supervisor.request",
            "id": "req1",
            "createdAt": "2026-08-14T00:00:00.000Z",
            "reason": "need_decision",
            "message": "Ship?",
            "orchestratorSessionId": "session-a",
            "runId": "runx",
        });
        std::fs::write(
            channel.join("requests").join("req1.json"),
            request.to_string(),
        )
        .unwrap();

        // Wrong session never sees it.
        let pending = parent_supervisor_action("pending", None, None, "session-b", &root);
        let text = pending["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("(none)"), "{text}");

        // Owning session sees it and can reply.
        let pending = parent_supervisor_action("pending", None, None, "session-a", &root);
        assert!(pending["content"][0]["text"].as_str().unwrap().contains("req1"));
        let reply = parent_supervisor_action(
            "reply",
            Some("req1"),
            Some("Ship it."),
            "session-a",
            &root,
        );
        assert_eq!(reply["isError"], Value::Bool(false));
        assert!(channel.join("replies").join("req1.json").exists());
        assert!(!channel.join("requests").join("req1.json").exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
