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

use serde_json::{Value, json};

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
    let _ = std::fs::create_dir_all(dir.join("terminal"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
}

/// System-wide monotonic milliseconds (CLOCK_MONOTONIC): comparable across
/// the child/parent processes on one boot and immune to wall-clock jumps.
pub fn monotonic_millis() -> u64 {
    #[cfg(unix)]
    {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // Safety: clock_gettime writes the provided timespec.
        if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) } == 0 {
            return (ts.tv_sec as u64)
                .saturating_mul(1000)
                .saturating_add(ts.tv_nsec as u64 / 1_000_000);
        }
        now_millis()
    }
    #[cfg(not(unix))]
    {
        now_millis()
    }
}

/// S36: persist the latest `contact_supervisor(progress_update)` text.
pub fn write_progress(channel_dir: &Path, run_id: &str, message: &str) {
    let _ = ensure_channel_dir(channel_dir);
    let payload = json!({
        "runId": run_id,
        "text": message.trim(),
        "receivedAt": now_millis(),
        "receivedAtMonotonic": monotonic_millis(),
    });
    let path = channel_dir.join("progress.json");
    let _ = std::fs::write(path, payload.to_string());
}

/// S36: the latest progress text with wall-clock (display) and monotonic
/// (age) receive times.
pub struct ProgressSnapshot {
    pub text: String,
    #[allow(dead_code)] // wall clock is display-only; age uses monotonic
    pub received_at: u64,
    pub received_at_monotonic: u64,
}

pub fn latest_progress(channel_dir: &Path) -> Option<ProgressSnapshot> {
    let raw = std::fs::read_to_string(channel_dir.join("progress.json")).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    let text = value
        .get("text")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())?
        .to_string();
    let received_at = value
        .get("receivedAt")
        .and_then(Value::as_u64)
        .unwrap_or_else(now_millis);
    // Records written before the monotonic field fall back to the wall
    // value (best effort; new records always carry the monotonic one).
    let received_at_monotonic = value
        .get("receivedAtMonotonic")
        .and_then(Value::as_u64)
        .unwrap_or(received_at);
    Some(ProgressSnapshot {
        text,
        received_at,
        received_at_monotonic,
    })
}

/// S36: drop progress files for a finished run.
pub fn clear_progress_for_run(run_id: &str) {
    let Ok(entries) = std::fs::read_dir(channels_root()) else {
        return;
    };
    for channel in entries.flatten() {
        let path = channel.path().join("progress.json");
        let owned = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
            .and_then(|value| {
                value
                    .get("runId")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .as_deref()
            == Some(run_id);
        if owned {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn ensure_channel_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir.join("requests"))?;
    std::fs::create_dir_all(dir.join("replies"))?;
    std::fs::create_dir_all(dir.join("terminal"))
}

/// `formatChildMessage` header (L151): reason title + run context lines.
pub fn format_child_message(
    reason: &str,
    run_id: &str,
    agent: &str,
    child_index: usize,
    message: &str,
) -> String {
    let title = match reason {
        "need_decision" => "Subagent needs a decision",
        "interview_request" => "Subagent requests structured input",
        _ => "Subagent progress update",
    };
    format!("{title}\nRun: {run_id}\nAgent: {agent}\nChild index: {child_index}\n\n{message}")
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
        if let Ok(raw) = std::fs::read_to_string(&path)
            && let Ok(value) = serde_json::from_str::<Value>(&raw)
        {
            requests.push(value);
        }
    }
    requests.sort_by(|a, b| a["createdAt"].as_str().cmp(&b["createdAt"].as_str()));
    requests
}

/// Drop every pending request owned by `run_id` when the run reaches a
/// terminal state or the host shuts down. A dead child cannot consume a
/// reply, and a stale blocking ask would otherwise keep `subagent_wait` and
/// headless auto-drain yielding forever (the `eb91202f` shutdown half).
///
/// A tombstone is kept in `terminal/<id>.json` so a late reply can be told
/// why the request is no longer answerable (S18).
pub fn close_requests_for_run(run_id: &str) -> usize {
    close_requests_for_run_with_kind(run_id, "closed")
}

pub fn close_requests_for_run_with_kind(run_id: &str, kind: &str) -> usize {
    let Ok(entries) = std::fs::read_dir(channels_root()) else {
        return 0;
    };
    let mut closed = 0;
    for channel in entries.flatten() {
        let requests = channel.path().join("requests");
        let Ok(files) = std::fs::read_dir(&requests) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            let request = std::fs::read_to_string(&path)
                .ok()
                .and_then(|raw| serde_json::from_str::<Value>(&raw).ok());
            let Some(request) = request else {
                continue;
            };
            if request["runId"].as_str() != Some(run_id) {
                continue;
            }
            let Some(id) = request["id"].as_str() else {
                continue;
            };
            if let Err(error) = crate::paths::ensure_safe_component(id, "supervisor request id") {
                tracing::warn!(%error, "skipping unsafe supervisor request id");
                continue;
            }
            if std::fs::remove_file(&path).is_err() {
                continue;
            }
            let _ = ensure_channel_dir(&channel.path());
            let tombstone = json!({
                "id": id,
                "runId": run_id,
                "orchestratorSessionId": request["orchestratorSessionId"],
                "reason": request["reason"],
                "childIndex": request["childIndex"],
                "terminalKind": kind,
                "closedAt": crate::artifacts::format_iso8601(now_millis()),
            });
            let _ = std::fs::write(
                channel.path().join("terminal").join(format!("{id}.json")),
                tombstone.to_string(),
            );
            closed += 1;
        }
    }
    closed
}

/// Find the terminal tombstone for `request_id` under `root`.
fn terminal_request(root: &Path, request_id: &str) -> Option<Value> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return None;
    };
    for channel in entries.flatten() {
        let path = channel
            .path()
            .join("terminal")
            .join(format!("{request_id}.json"));
        if let Ok(raw) = std::fs::read_to_string(&path)
            && let Ok(value) = serde_json::from_str::<Value>(&raw)
        {
            return Some(value);
        }
    }
    None
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
        let orchestrator_session_id =
            std::env::var(crate::launch::args::SUBAGENT_ORCHESTRATOR_SESSION_ID_ENV)
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
            return error_result(
                "reason must be need_decision, interview_request, or progress_update.",
            );
        }
        let message = params
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let formatted =
            format_child_message(reason, &self.run_id, &self.agent, self.child_index, message);
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
        // progress_update (L265-270 + 3d78e6ec at v0.70): a non-blocking
        // note. S36 (#3230): persist the latest text so the widget/status can
        // show it with an age; it never becomes a parent turn.
        if reason == "progress_update" {
            write_progress(&self.channel_dir, &self.run_id, message);
            return json!({
                "content": [{ "type": "text", "text": "Progress update recorded." }],
                "details": { "mode": "single", "results": [] },
                "isError": false,
            });
        }
        // need_decision / interview_request DO reach the parent — see
        // `blocking_requests_for_session` (async wake) and the foreground
        // detach path in `runner/foreground.rs` (issue #55).
        if reason == "progress_update" {
            return ok_result("Progress update delivered.");
        }
        let serialized = request.to_string();
        if serialized.len() > MAX_REQUEST_BYTES {
            return error_result(
                "supervisor request exceeds the 64 KiB channel limit; shorten the message.",
            );
        }
        ensure_channel(&self.channel_dir);
        let request_path = self
            .channel_dir
            .join("requests")
            .join(format!("{request_id}.json"));
        if let Err(error) = std::fs::write(&request_path, serialized) {
            return error_result(&format!("failed to write the supervisor request: {error}"));
        }
        // Poll the reply inbox (≤500ms interval, 10min timeout, L272-279).
        let reply_path = self
            .channel_dir
            .join("replies")
            .join(format!("{request_id}.json"));
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
                    if let Some(start) = message.find("```")
                        && let Some(body) = message[start + 3..]
                            .strip_prefix("json\n")
                            .or_else(|| message[start + 3..].split_once('\n').map(|(_, rest)| rest))
                        && let Some(end) = body.find("```")
                        && let Ok(parsed) = serde_json::from_str::<Value>(body[..end].trim())
                    {
                        return json!({
                            "content": [{ "type": "text", "text": message }],
                            "details": { "structured": parsed },
                            "isError": false,
                        });
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
        "details": { "mode": "single", "results": [] },
        "isError": false,
    })
}

fn error_result(text: &str) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }],
        "details": { "mode": "single", "results": [] },
        "isError": true,
    })
}

/// Pending blocking supervisor asks for one child (FR-A / #1980,
/// `steerAsyncRun`'s `findPendingAsks` seam): requests still sitting in
/// `requests/` that expect a reply (need_decision / interview_request —
/// `progress_update` writes return immediately and never block) for the
/// given run + child index. Unanswered asks mean the child cannot consume a
/// steer, so the steer gate reports them instead of writing the inbox.
pub fn pending_asks(run_id: &str, child_index: usize) -> Vec<Value> {
    let root = channels_root();
    let Ok(channel_entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut asks = Vec::new();
    for channel in channel_entries.flatten() {
        for request in read_requests(&channel.path()) {
            let blocking = request["reason"].as_str() == Some("need_decision")
                || request["reason"].as_str() == Some("interview_request");
            if !blocking {
                continue;
            }
            if request["runId"].as_str() != Some(run_id) {
                continue;
            }
            if request["childIndex"].as_u64() != Some(child_index as u64) {
                continue;
            }
            asks.push(request);
        }
    }
    asks.sort_by(|a, b| a["createdAt"].as_str().cmp(&b["createdAt"].as_str()));
    asks
}

/// Run-level variant (#1315 attention, `needsAttention` is run-scoped in
/// upstream's wait): every blocking ask under the run regardless of child
/// index — a multi-child run blocked on child ≥1 must still stop the wait.
pub fn pending_asks_for_run(run_id: &str) -> Vec<Value> {
    let root = channels_root();
    let Ok(channel_entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut asks = Vec::new();
    for channel in channel_entries.flatten() {
        for request in read_requests(&channel.path()) {
            let blocking = request["reason"].as_str() == Some("need_decision")
                || request["reason"].as_str() == Some("interview_request");
            if blocking && request["runId"].as_str() == Some(run_id) {
                asks.push(request);
            }
        }
    }
    asks.sort_by(|a, b| a["createdAt"].as_str().cmp(&b["createdAt"].as_str()));
    asks
}

/// TE38 W7 (#2202 @ b72714de, `hasPendingRequests`): does this orchestrator
/// session own any unanswered blocking ask (need_decision /
/// interview_request)? The headless agent_end drain consults this and yields
/// — a headless parent at turn end cannot answer asks, so waiting would only
/// burn the drain budget while children block (upstream breaks the drain
/// loop on a pending request).
pub fn has_pending_blocking_requests(orchestrator_session_id: &str) -> bool {
    let root = channels_root();
    let Ok(channel_entries) = std::fs::read_dir(&root) else {
        return false;
    };
    for channel in channel_entries.flatten() {
        for request in read_requests(&channel.path()) {
            let blocking = request["reason"].as_str() == Some("need_decision")
                || request["reason"].as_str() == Some("interview_request");
            if blocking && request_matches_session(&request, orchestrator_session_id) {
                return true;
            }
        }
    }
    false
}

/// S12: move pending asks to the successor session after a take-over switch.
pub fn reassign_owner(old_session: &str, new_session: &str) {
    if old_session.is_empty() || new_session.is_empty() || old_session == new_session {
        return;
    }
    let Ok(channels) = std::fs::read_dir(channels_root()) else {
        return;
    };
    for channel in channels.flatten() {
        let requests = channel.path().join("requests");
        let Ok(files) = std::fs::read_dir(&requests) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(mut request) = serde_json::from_str::<Value>(&raw) else {
                continue;
            };
            if request["orchestratorSessionId"].as_str() != Some(old_session) {
                continue;
            }
            request["orchestratorSessionId"] = json!(new_session);
            let _ = std::fs::write(&path, request.to_string());
        }
    }
}

/// Blocking asks owned by this orchestrator session across every channel
/// (upstream `refreshPendingRequests` + `requestMatchesOwner` projection
/// for delivery): reply-expecting requests only, oldest first. This is
/// the async-run wake seam — the fleet refresh loop scans it every tick
/// and `sendMessage`s each new ask into the parent session with
/// `triggerTurn: true` (native-supervisor-channel.ts `poll()`, L706-757;
/// issue #55: the port originally dropped this entire delivery face, so
/// an idle parent never learned about a blocked child).
pub fn blocking_requests_for_session(session_id: &str) -> Vec<Value> {
    let root = channels_root();
    let mut asks = Vec::new();
    let Ok(channel_entries) = std::fs::read_dir(&root) else {
        return asks;
    };
    for channel in channel_entries.flatten() {
        for request in read_requests(&channel.path()) {
            let blocking = request["reason"].as_str() == Some("need_decision")
                || request["reason"].as_str() == Some("interview_request");
            if blocking && request_matches_session(&request, session_id) {
                asks.push(request);
            }
        }
    }
    asks.sort_by(|a, b| a["createdAt"].as_str().cmp(&b["createdAt"].as_str()));
    asks
}

/// The first unanswered blocking ask in one child's channel (the
/// foreground detach watcher's scan — the channel is per-child, so no
/// session or run filtering applies; the reply path still enforces
/// session ownership).
pub fn first_blocking_ask_in(channel_dir: &Path) -> Option<Value> {
    let mut requests = read_requests(channel_dir);
    requests.sort_by(|a, b| a["createdAt"].as_str().cmp(&b["createdAt"].as_str()));
    requests.into_iter().find(|request| {
        request["reason"].as_str() == Some("need_decision")
            || request["reason"].as_str() == Some("interview_request")
    })
}

/// `supervisorReplyHint` (supervisor-ui.ts:50-52): the exact call shape the
/// parent should make to answer.
pub fn reply_hint(request_id: &str) -> String {
    format!(
        "subagent_supervisor({{ action: \"reply\", replyTo: \"{request_id}\", message: \"...\" }})"
    )
}

/// `requestVisibleText` (supervisor-ui.ts:493-510): the child's formatted
/// message (title + run/agent/child context + body — the child wrote it
/// via `format_child_message`) plus the reply instruction. This is the
/// `sendMessage` content the parent model reads.
pub fn request_visible_text(request: &Value) -> String {
    let body = request["message"].as_str().unwrap_or("");
    let request_id = request["id"].as_str().unwrap_or("?");
    format!("{body}\n\nReply with: {}", reply_hint(request_id))
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
                        // #2229/#2230 + 3d78e6ec (v0.70): progress updates never
                        // reach the parent — only reply-expecting requests
                        // surface in the pending listing.
                        if request["reason"].as_str() == Some("progress_update") {
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
            // The id joins filesystem paths — reject path-shaped input (C1).
            if let Err(error) = crate::paths::ensure_safe_component(reply_to, "replyTo") {
                return error_result(&error);
            }
            // Find the matching request across channels; only requests this
            // session owns are visible (upstream resolves replies through the
            // session-filtered pending map, native-supervisor-channel.ts
            // 571-576 + 674).
            if let Ok(channel_entries) = std::fs::read_dir(channels_root) {
                let mut wrong_session = false;
                for channel in channel_entries.flatten() {
                    let request_path = channel
                        .path()
                        .join("requests")
                        .join(format!("{reply_to}.json"));
                    if !request_path.exists() {
                        continue;
                    }
                    let Ok(raw) = std::fs::read_to_string(&request_path) else {
                        continue;
                    };
                    let Ok(request) = serde_json::from_str::<Value>(&raw) else {
                        continue;
                    };
                    if !request_matches_session(&request, orchestrator_session_id) {
                        wrong_session = true;
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
                // S18: a late reply to a request the run already closed gets
                // the terminal reason instead of the generic "no match".
                if let Some(tombstone) = terminal_request(channels_root, reply_to) {
                    if request_matches_session(&tombstone, orchestrator_session_id) {
                        let kind = tombstone["terminalKind"].as_str().unwrap_or("closed");
                        return error_result(&format!(
                            "Supervisor request '{reply_to}' is already {kind}; it cannot be answered."
                        ));
                    }
                    return error_result(&format!(
                        "Supervisor request '{reply_to}' belongs to another session."
                    ));
                }
                if wrong_session {
                    return error_result(&format!(
                        "Supervisor request '{reply_to}' belongs to another session."
                    ));
                }
            }
            error_result(&format!(
                "No pending supervisor request matches '{reply_to}'."
            ))
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
    if let Some(tool_list) = tools.as_mut()
        && !tool_list.iter().any(|t| t == "contact_supervisor")
    {
        tool_list.push("contact_supervisor".to_string());
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
        assert!(
            tools
                .as_ref()
                .unwrap()
                .contains(&"contact_supervisor".to_string())
        );
        assert!(prompt.contains("Intercom orchestration channel:"));

        apply_intercom_bridge("always", None, &mut tools, &mut prompt);
        // Idempotent: no duplicate tool or prompt block.
        assert_eq!(
            tools
                .as_ref()
                .unwrap()
                .iter()
                .filter(|t| *t == "contact_supervisor")
                .count(),
            1
        );
        assert_eq!(prompt.matches("Intercom orchestration channel:").count(), 1);
    }

    #[test]
    fn closing_a_run_removes_only_its_requests() {
        let run_id = format!("close-run-{}", std::process::id());
        let other_run = format!("keep-run-{}", std::process::id());
        for (run, id) in [(&run_id, "r1"), (&other_run, "r2")] {
            let channel = channels_root().join(format!("close-test-{run}-worker-0"));
            let _ = std::fs::remove_dir_all(&channel);
            ensure_channel(&channel);
            std::fs::write(
                channel.join("requests").join(format!("{id}.json")),
                json!({ "id": id, "reason": "need_decision", "runId": run,
                        "orchestratorSessionId": "session-close", "createdAt": "2026-09-26T00:00:00.000Z" })
                    .to_string(),
            )
            .unwrap();
        }
        assert_eq!(close_requests_for_run(&run_id), 1);
        assert_eq!(close_requests_for_run(&run_id), 0, "second pass is a no-op");
        // The other run's request survives.
        let other_channel = channels_root().join(format!("close-test-{other_run}-worker-0"));
        assert_eq!(read_requests(&other_channel).len(), 1);
        let _ = std::fs::remove_dir_all(format!(
            "{}/close-test-{run_id}-worker-0",
            channels_root().display()
        ));
        let _ = std::fs::remove_dir_all(format!(
            "{}/close-test-{other_run}-worker-0",
            channels_root().display()
        ));
    }

    /// TE38 T1 (#2327 @ b72714de): supervisor scanning is fail-soft — a
    /// vanished channel root or channel directory reads as "no requests",
    /// never a panic or an error surface. Rust's read_dir treats every
    /// failure uniformly (upstream needed the win32 `UNKNOWN` carve-out;
    /// the uniform behavior subsumes it).
    #[test]
    fn scan_tolerates_vanished_directories() {
        // Missing root: "pending" reads empty, no panic.
        let missing_root = channels_root().join(format!("vanished-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&missing_root);
        let pending =
            parent_supervisor_action("pending", None, None, "session-vanished", &missing_root);
        assert_eq!(pending["isError"], Value::Bool(false));
        assert!(
            pending["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("(none)")
        );
        // Session-scoped probe over the (missing) global root: false.
        assert!(!has_pending_blocking_requests("session-vanished"));
        // A channel dir that disappears between listing and reading: the
        // flatten() + read_requests chain skips it silently. The channel
        // sits directly under the global root (the scan is one level deep).
        let channel =
            channels_root().join(format!("vanish-mid-runz-worker-0-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&channel);
        ensure_channel(&channel);
        std::fs::write(
            channel.join("requests").join("r1.json"),
            json!({ "id": "r1", "reason": "need_decision", "runId": "runz",
                    "orchestratorSessionId": "session-mid", "createdAt": "2026-09-26T00:00:00.000Z" })
                .to_string(),
        )
        .unwrap();
        assert!(has_pending_blocking_requests("session-mid"));
        // Remove the whole channel (simulating the race): scans stay quiet.
        let _ = std::fs::remove_dir_all(&channel);
        assert!(!has_pending_blocking_requests("session-mid"));
        assert!(pending_asks("runz", 0).is_empty());
        let _ = std::fs::remove_dir_all(&channel);
    }

    /// Best-effort sweep of same-prefix test channels (self-cleaning
    /// tests: the projection reads the whole root).
    fn sweep_channel_prefix(prefix: &str) {
        let root = channels_root();
        let Ok(entries) = std::fs::read_dir(&root) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(prefix) {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }

    /// #55 delivery seam: `blocking_requests_for_session` projects only
    /// reply-expecting requests owned by the session, oldest first;
    /// `request_visible_text` carries the reply hint with the request id.
    /// Channels sit directly under the global root (the scan is one level
    /// deep, matching production layout).
    #[test]
    fn session_ask_projection_and_visible_text() {
        // Random suffix + prefix sweep: the scan reads every channel
        // under the root, so a failed earlier run must not leave a twin
        // channel behind (the global root also accumulates unrelated
        // leftovers — upstream's stale-channel cleanup is not ported).
        let suffix = crate::runner::budget::random_run_id();
        sweep_channel_prefix("runq-worker-0-askproj-");
        let channel = channels_root().join(format!("runq-worker-0-askproj-{suffix}"));
        let _ = std::fs::remove_dir_all(&channel);
        ensure_channel(&channel);
        let base = |id: &str, reason: &str, created: &str, session: &str| {
            json!({
                "type": "subagent.supervisor.request",
                "id": id,
                "createdAt": created,
                "reason": reason,
                "message": format!("Subagent needs a decision\nRun: runq\nAgent: worker\nChild index: 0\n\nShip or hold? ({id})"),
                "orchestratorSessionId": session,
                "runId": "runq",
                "agent": "worker",
                "childIndex": 0,
            })
        };
        for (name, reason, created, session) in [
            (
                "late",
                "need_decision",
                "2026-09-29T00:00:02.000Z",
                "session-a",
            ),
            (
                "early",
                "need_decision",
                "2026-09-29T00:00:01.000Z",
                "session-a",
            ),
            (
                "note",
                "progress_update",
                "2026-09-29T00:00:00.000Z",
                "session-a",
            ),
            (
                "foreign",
                "need_decision",
                "2026-09-29T00:00:00.500Z",
                "session-b",
            ),
        ] {
            std::fs::write(
                channel.join("requests").join(format!("{name}.json")),
                base(name, reason, created, session).to_string(),
            )
            .unwrap();
        }

        // Blocking-only + session-owned + oldest-first ordering.
        let asks = blocking_requests_for_session("session-a");
        let ids: Vec<&str> = asks.iter().filter_map(|a| a["id"].as_str()).collect();
        assert_eq!(ids, vec!["early", "late"], "{ids:?}");
        // Cross-session isolation: session-b sees only its own ask.
        let foreign = blocking_requests_for_session("session-b");
        let foreign_ids: Vec<&str> = foreign.iter().filter_map(|a| a["id"].as_str()).collect();
        assert_eq!(foreign_ids, vec!["foreign"], "{foreign_ids:?}");

        // Visible text: child-formatted body + the reply hint naming the id.
        let text = request_visible_text(&asks[0]);
        assert!(text.contains("Subagent needs a decision"), "{text}");
        assert!(text.contains("Ship or hold? (early)"), "{text}");
        assert!(
            text.contains(
                "Reply with: subagent_supervisor({ action: \"reply\", replyTo: \"early\""
            ),
            "{text}"
        );

        // Single-channel watcher scan: the earliest blocking ask in the
        // channel regardless of owning session (per-child channel; the
        // reply path enforces session ownership) — 00:00:00.500 "foreign"
        // precedes "early"; the progress note is never a candidate.
        let first = first_blocking_ask_in(&channel).unwrap();
        assert_eq!(first["id"], json!("foreign"));
        let _ = std::fs::remove_dir_all(&channel);
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
        assert!(
            pending["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("req1")
        );
        let reply =
            parent_supervisor_action("reply", Some("req1"), Some("Ship it."), "session-a", &root);
        assert_eq!(reply["isError"], Value::Bool(false));
        assert!(channel.join("replies").join("req1.json").exists());
        assert!(!channel.join("requests").join("req1.json").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn late_reply_reports_the_terminal_kind() {
        let root = channels_root().join(format!("late-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let channel = root.join("runy-worker-0");
        ensure_channel(&channel);
        std::fs::write(
            channel.join("terminal").join("req9.json"),
            json!({
                "id": "req9",
                "runId": "runy",
                "orchestratorSessionId": "session-a",
                "terminalKind": "stopped",
            })
            .to_string(),
        )
        .unwrap();
        let reply =
            parent_supervisor_action("reply", Some("req9"), Some("too late"), "session-a", &root);
        assert_eq!(reply["isError"], Value::Bool(true));
        assert!(
            reply["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("already stopped"),
            "{reply}"
        );
        let foreign =
            parent_supervisor_action("reply", Some("req9"), Some("too late"), "session-b", &root);
        assert!(
            foreign["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("belongs to another session"),
            "{foreign}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
