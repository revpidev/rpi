//! Trusted recovery records and runner launchers (V19-03 S11/S42).
//!
//! Background runs keep two kinds of state:
//!
//! ```text
//! <temp>/rpi-subagents-<scope>/async-subagent-launches/<run-id>/recovery.json  # parent-owned
//! <temp>/rpi-subagents-<scope>/async-subagent-runs/<run-id>/status.json        # diagnostic
//! ```
//!
//! The recovery record sits beside the runs root in a folder the child never
//! binds or writes; resume/auto-resume reads it and treats `status.json` as
//! diagnostics only. Records written before this format existed are refused
//! with a relaunch/`reuseFrom` hint (BREAKING, documented in the v0.1.9
//! changes). The record also carries the resolved runner launcher and the
//! agent definition digest, so a resume starts the same approved definition.
//!
//! rpi has no separate fork generation in the host API: the parent session id
//! (a fork gets a new session file/id) is the owner generation.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::agents::discover::AgentConfig;
use crate::config::ExtensionConfig;

/// Record format version.
pub const RECOVERY_VERSION: u32 = 1;

/// `runnerLaunchers` name pattern (shared/types.ts @ feb6da00).
pub fn valid_launcher_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_alphabetic() && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// The launcher recorded for a child (and required by `requireLauncher`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LauncherRecord {
    pub name: String,
    /// 64-hex SHA-256 of the agent definition the launcher approved.
    pub definition_digest: String,
}

/// `options.requireLauncher` (public-execution.ts:50 @ feb6da00, #3163).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequiredLauncher {
    pub name: String,
    pub definition_digest: String,
}

impl RequiredLauncher {
    /// Parse `{name, definitionDigest}` from raw call params.
    pub fn parse(value: &Value) -> Result<Self, String> {
        let Some(object) = value.as_object() else {
            return Err("requireLauncher must be { name, definitionDigest }.".to_string());
        };
        if object
            .keys()
            .any(|key| key != "name" && key != "definitionDigest")
        {
            return Err("requireLauncher must be { name, definitionDigest }.".to_string());
        }
        let name = object
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| valid_launcher_name(name))
            .ok_or_else(|| "requireLauncher.name must be a valid launcher name.".to_string())?;
        let digest = object
            .get("definitionDigest")
            .and_then(Value::as_str)
            .filter(|digest| is_sha256_hex(digest))
            .ok_or_else(|| {
                "requireLauncher.definitionDigest must be a 64-character lowercase hex digest."
                    .to_string()
            })?;
        Ok(Self {
            name: name.to_string(),
            definition_digest: digest.to_string(),
        })
    }
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

/// One child's trusted launch facts.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryChild {
    pub index: u32,
    pub agent: String,
    pub agent_path: String,
    pub definition_digest: String,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    /// The recorded launcher; `launcher_recorded` distinguishes an explicit
    /// "no launcher" (`true` + `None`) from a record written before the field
    /// existed (`false`, resume falls back to the agent file's launcher).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launcher: Option<LauncherRecord>,
    #[serde(default)]
    pub launcher_recorded: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_launcher: Option<RequiredLauncher>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_key: Option<String>,
    #[serde(default)]
    pub state: String,
}

/// The parent-owned trusted record for one async run.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrustedRecoveryRecord {
    pub version: u32,
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// rpi's owner generation: the parent session id (forks get new ids).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_generation: Option<String>,
    pub mode: String,
    pub state: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub children: Vec<RecoveryChild>,
}

impl TrustedRecoveryRecord {
    pub fn child(&self, index: usize) -> Option<&RecoveryChild> {
        self.children
            .iter()
            .find(|child| child.index == index as u32)
    }
}

/// Where the parent-owned launch folder lives for a run id.
pub fn launch_dir(run_id: &str) -> PathBuf {
    crate::paths::temp_root_dir()
        .join("async-subagent-launches")
        .join(run_id)
}

pub fn recovery_path(run_id: &str) -> PathBuf {
    launch_dir(run_id).join("recovery.json")
}

fn now_iso() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0);
    crate::artifacts::format_iso8601(millis)
}

/// Serializes read-modify-write updates (parallel/chain children launch
/// concurrently on the plugin runtime).
static RECORD_LOCK: Mutex<()> = Mutex::new(());

/// Write one record atomically (0600 on unix): temp file in the same folder
/// then rename, so a crash never leaves a half-written trusted record.
fn write_record(record: &TrustedRecoveryRecord) -> Result<(), String> {
    let path = recovery_path(&record.run_id);
    let Some(parent) = path.parent() else {
        return Err("recovery record path has no parent".to_string());
    };
    crate::paths::create_private_dir_all(parent)
        .map_err(|error| format!("Failed to create recovery folder: {error}"))?;
    let body = serde_json::to_vec_pretty(record)
        .map_err(|error| format!("Failed to serialize recovery record: {error}"))?;
    let temp = parent.join(format!(
        ".recovery-{}-{}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::write(&temp, body)
        .map_err(|error| format!("Failed to write recovery record: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&temp, &path).map_err(|error| {
        let _ = std::fs::remove_file(&temp);
        format!("Failed to persist recovery record: {error}")
    })
}

/// Seed the record when an async run starts (before any child exists).
pub fn start_record(
    run_id: &str,
    mode: &str,
    parent_session_id: Option<&str>,
) -> Result<(), String> {
    let _guard = RECORD_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let now = now_iso();
    let record = TrustedRecoveryRecord {
        version: RECOVERY_VERSION,
        run_id: run_id.to_string(),
        parent_session_id: parent_session_id.map(str::to_string),
        owner_generation: parent_session_id.map(str::to_string),
        mode: mode.to_string(),
        state: "running".to_string(),
        created_at: now.clone(),
        updated_at: now,
        children: Vec::new(),
    };
    write_record(&record)
}

/// Read a record and validate its own identity. `Ok(None)` = no record
/// (legacy run); malformed records are errors.
pub fn read_record(run_id: &str) -> Result<Option<TrustedRecoveryRecord>, String> {
    let path = recovery_path(run_id);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "Failed to read recovery record '{}': {error}",
                path.to_string_lossy()
            ));
        }
    };
    let record: TrustedRecoveryRecord = serde_json::from_str(&raw).map_err(|error| {
        format!(
            "Invalid recovery record '{}': {error}",
            path.to_string_lossy()
        )
    })?;
    if record.version != RECOVERY_VERSION {
        return Err(format!(
            "Invalid recovery record '{}': unsupported version {}",
            path.to_string_lossy(),
            record.version
        ));
    }
    if record.run_id != run_id {
        return Err(format!(
            "Recovery record '{}' names run '{}', but the launch folder is '{}'.",
            path.to_string_lossy(),
            record.run_id,
            run_id
        ));
    }
    Ok(Some(record))
}

/// Read-modify-write helper.
fn update_record(
    run_id: &str,
    update: impl FnOnce(&mut TrustedRecoveryRecord),
) -> Result<(), String> {
    let _guard = RECORD_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let Some(mut record) = read_record(run_id)? else {
        return Ok(());
    };
    update(&mut record);
    record.updated_at = now_iso();
    write_record(&record)
}

/// Record one child's trusted launch facts (idempotent by index).
pub fn record_child(run_id: &str, child: RecoveryChild) -> Result<(), String> {
    update_record(run_id, |record| {
        record.children.retain(|entry| entry.index != child.index);
        record.children.push(child);
        record.children.sort_by_key(|entry| entry.index);
    })
}

/// Update a child's derived session file and state after it settles.
pub fn update_child_session(run_id: &str, index: u32, session_file: Option<&Path>, state: &str) {
    let _ = update_record(run_id, |record| {
        if let Some(child) = record
            .children
            .iter_mut()
            .find(|child| child.index == index)
        {
            if let Some(session_file) = session_file {
                child.session_file = Some(session_file.to_string_lossy().to_string());
            }
            child.state = state.to_string();
        }
    });
}

/// S12: move the trusted owner generation after a take-over switch.
pub fn reassign_owner(old_session: &str, new_session: &str) {
    if old_session.is_empty() || new_session.is_empty() || old_session == new_session {
        return;
    }
    let root = crate::paths::temp_root_dir().join("async-subagent-launches");
    let Ok(entries) = std::fs::read_dir(&root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path().join("recovery.json");
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(mut record) = serde_json::from_str::<TrustedRecoveryRecord>(&raw) else {
            continue;
        };
        if record.parent_session_id.as_deref() != Some(old_session) {
            continue;
        }
        record.parent_session_id = Some(new_session.to_string());
        record.owner_generation = Some(new_session.to_string());
        let _ = write_record(&record);
    }
}

/// Update the run-level state (interrupt → paused, resume → running).
pub fn set_record_state(run_id: &str, state: &str) {
    let _ = update_record(run_id, |record| {
        record.state = state.to_string();
    });
}

/// Terminal state projection for the run.
pub fn finish_record(run_id: &str, state: &str) {
    let _ = update_record(run_id, |record| {
        record.state = state.to_string();
    });
}

/// Canonical agent definition digest (`agentDefinitionDigest`,
/// launch-contract.ts @ feb6da00): SHA-256 over the behavior-defining fields
/// in a fixed order. Comments/formatting of the source file do not count;
/// every field that can change what the child runs does.
pub fn agent_definition_digest(agent: &AgentConfig) -> String {
    let thinking = match &agent.thinking {
        crate::agents::discover::ThinkingSpec::Level(level) => json!({ "level": level }),
        crate::agents::discover::ThinkingSpec::Disabled => json!({ "disabled": true }),
        crate::agents::discover::ThinkingSpec::Unset => Value::Null,
    };
    let canonical = json!({
        "name": agent.name,
        "description": agent.description,
        "tools": agent.tools,
        "excludeTools": agent.exclude_tools,
        "mcpDirectTools": agent.mcp_direct_tools,
        "mutationTools": agent.mutation_tools,
        "model": agent.model,
        "modelProvider": agent.model_provider,
        "thinking": thinking,
        "systemPromptMode": agent.system_prompt_mode,
        "inheritProjectContext": agent.inherit_project_context,
        "inheritGlobalContext": agent.inherit_global_context,
        "inheritSkills": agent.inherit_skills,
        "defaultContext": agent.default_context.map(|context| match context {
            crate::agents::discover::ContextMode::Fresh => "fresh",
            crate::agents::discover::ContextMode::Fork => "fork",
        }),
        "defaultAsync": agent.default_async,
        "defaultTimeoutMs": agent.default_timeout_ms,
        "systemPrompt": agent.system_prompt,
        "skills": agent.skills,
        "extensions": agent.extensions,
        "subagentOnlyExtensions": agent.subagent_only_extensions,
        "output": agent.output,
        "outputMode": agent.output_mode,
        "allowNestedSubagents": agent.allow_nested_subagents,
        "allowedAgents": agent.allowed_agents,
        "launcher": agent.launcher,
        "maxSubagentDepth": agent.max_subagent_depth,
        "acceptanceRole": agent.acceptance_role,
        "memory": agent.memory.as_ref().map(|memory| json!({
            "scope": memory.scope,
            "path": memory.path,
        })),
    });
    let bytes = serde_json::to_vec(&canonical).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// The launcher an agent launch must run under, or the reason it cannot.
/// `recorded` is `Some` for a resume: the trusted record decides (missing
/// field → current agent launcher; explicit none → unwrapped).
pub fn resolve_launcher(
    config: &ExtensionConfig,
    agent: &AgentConfig,
    recorded: Option<&LauncherRecord>,
    recorded_explicitly: bool,
) -> Result<Option<(LauncherRecord, Vec<String>)>, String> {
    let record = if recorded_explicitly {
        recorded.cloned()
    } else {
        agent.launcher.as_deref().map(|name| LauncherRecord {
            name: name.to_string(),
            definition_digest: agent_definition_digest(agent),
        })
    };
    let Some(record) = record else {
        return Ok(None);
    };
    let Some(argv) = config.runner_launcher(&record.name) else {
        return Err(format!(
            "Agent '{}' uses launcher '{}', which is not defined in runnerLaunchers in the user subagent config.",
            agent.name, record.name
        ));
    };
    if argv.is_empty() {
        return Err(format!(
            "runnerLaunchers.{} must not be empty.",
            record.name
        ));
    }
    Ok(Some((record, argv)))
}

/// Verify `requireLauncher` against the resolved agent/launcher.
pub fn check_required_launcher(
    agent: &AgentConfig,
    launcher: Option<&LauncherRecord>,
    required: &RequiredLauncher,
) -> Result<(), String> {
    let Some(launcher) = launcher else {
        return Err(format!(
            "Agent '{}' has no launcher, but the call requires launcher '{}'.",
            agent.name, required.name
        ));
    };
    if launcher.name != required.name {
        return Err(format!(
            "Agent '{}' uses launcher '{}', but the call requires launcher '{}'.",
            agent.name, launcher.name, required.name
        ));
    }
    if launcher.definition_digest != required.definition_digest {
        return Err(format!(
            "Agent '{}' definition digest is {}, but the call requires {}; the agent definition changed after it was checked.",
            agent.name, launcher.definition_digest, required.definition_digest
        ));
    }
    Ok(())
}

/// `requireLauncher` is only valid on a top-level single-agent call
/// (runner-launcher.ts:5 @ feb6da00).
pub fn validate_require_launcher_shape(
    params: &serde_json::Map<String, Value>,
) -> Result<(), String> {
    if params.get("requireLauncher").is_none() {
        return Ok(());
    }
    if params.contains_key("action")
        || params.contains_key("tasks")
        || params.contains_key("steps")
        || params.contains_key("chain")
        || params.contains_key("parallel")
        || params.contains_key("workflow")
        || params.contains_key("resume")
        || params.get("agent").and_then(Value::as_str).is_none()
        || params.get("task").and_then(Value::as_str).is_none()
    {
        return Err(
            "requireLauncher is supported only on a top-level single-agent call (agent + task); it cannot be combined with tasks, chain, workflow, an action, resume, or a workflow child."
                .to_string(),
        );
    }
    Ok(())
}

/// Resume-side validation: load and verify the trusted record for `run_id`.
/// Returns the selected child's trusted facts or the refusal message.
pub fn trusted_resume_child(
    run_id: &str,
    current_session: Option<&str>,
    agents: &[AgentConfig],
    index: usize,
) -> Result<(TrustedRecoveryRecord, RecoveryChild), String> {
    let Some(record) = read_record(run_id)? else {
        return Err(format!(
            "Background run '{run_id}' has no trusted recovery record; it was started by an older rpi-subagents and cannot be resumed. Relaunch it (or start a new run with options.reuseFrom = \"{run_id}\")."
        ));
    };
    if let Some(current) = current_session {
        let owner = record
            .parent_session_id
            .as_deref()
            .or(record.owner_generation.as_deref());
        if owner != Some(current) {
            return Err(format!(
                "Async run '{run_id}' was not found in the active session."
            ));
        }
    }
    let Some(child) = record.child(index).cloned() else {
        return Err(format!(
            "Background run {run_id} has no trusted record for child {index}; resume cannot start it."
        ));
    };
    let agent = agents
        .iter()
        .find(|agent| agent.name == child.agent)
        .ok_or_else(|| {
            format!(
                "Background run {run_id} child {index} was launched with agent '{}', which is no longer available. Relaunch it instead.",
                child.agent
            )
        })?;
    let agent_path = Path::new(&child.agent_path);
    if !agent_path.is_file() {
        return Err(format!(
            "Background run {run_id} child {index} agent definition '{}' no longer exists; relaunch it instead.",
            child.agent_path
        ));
    }
    let digest = agent_definition_digest(agent);
    if digest != child.definition_digest {
        return Err(format!(
            "Background run {run_id} child {index} agent definition changed since launch (recorded {}, now {}); relaunch it instead of resuming.",
            child.definition_digest, digest
        ));
    }
    if !Path::new(&child.cwd).is_dir() {
        return Err(format!(
            "Background run {run_id} child {index} working directory '{}' no longer exists; resume is refused.",
            child.cwd
        ));
    }
    if let Some(launcher) = &child.launcher
        && !valid_launcher_name(&launcher.name)
    {
        return Err(format!(
            "Background run {run_id} child {index} has an invalid launcher record; resume is refused."
        ));
    }
    Ok((record, child))
}

/// Resume-side trusted facts for the next launch on this thread.
#[derive(Debug, Clone, Default)]
pub struct ResumeContext {
    /// `Some(None)` = the record explicitly says unwrapped.
    pub launcher: Option<Option<LauncherRecord>>,
    pub required_launcher: Option<RequiredLauncher>,
}

thread_local! {
    static RESUME_CONTEXT: std::cell::RefCell<Option<ResumeContext>> =
        const { std::cell::RefCell::new(None) };
}

/// Run `f` with the resume context visible to `RunCtx::from_host`.
pub fn with_resume_context<T>(context: Option<ResumeContext>, f: impl FnOnce() -> T) -> T {
    let previous = RESUME_CONTEXT.with(|slot| slot.replace(context));
    let result = f();
    RESUME_CONTEXT.with(|slot| {
        slot.replace(previous);
    });
    result
}

pub fn resume_context() -> Option<ResumeContext> {
    RESUME_CONTEXT.with(|slot| slot.borrow().clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::discover::{AgentConfig, AgentSource, ThinkingSpec};

    fn unique(prefix: &str) -> String {
        format!(
            "{prefix}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }

    fn agent(path: PathBuf) -> AgentConfig {
        AgentConfig {
            name: "worker".into(),
            local_name: "worker".into(),
            package_name: None,
            description: "does work".into(),
            aliases: None,
            tools: None,
            exclude_tools: Vec::new(),
            mcp_direct_tools: Vec::new(),
            mutation_tools: Vec::new(),
            model: Some("provider/model".into()),
            allowed_agents: None,
            thinking: ThinkingSpec::Level("high".into()),
            system_prompt_mode: "replace",
            inherit_project_context: false,
            inherit_global_context: false,
            inherit_skills: false,
            launcher: None,
            default_context: None,
            default_async: None,
            default_timeout_ms: None,
            system_prompt: "prompt".into(),
            source: AgentSource::User,
            file_path: path,
            skills: Vec::new(),
            extensions: None,
            subagent_only_extensions: None,
            output: None,
            output_mode: None,
            advertise: None,
            allow_nested_subagents: None,
            model_provider: None,
            default_reads: Vec::new(),
            default_progress: false,
            max_subagent_depth: None,
            disabled: None,
            acceptance_role: None,
            memory: None,
            frontmatter_fields: Default::default(),
        }
    }

    #[test]
    fn definition_digest_changes_with_behavior_fields_only() {
        let dir = std::env::temp_dir().join(unique("rpi-digest"));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("worker.md");
        std::fs::write(&path, "---\nname: worker\n---\nbody\n").unwrap();
        let base = agent(path.clone());
        let digest = agent_definition_digest(&base);
        assert_eq!(digest.len(), 64);

        let mut same = base.clone();
        same.file_path = dir.join("other-path.md");
        assert_eq!(
            agent_definition_digest(&same),
            digest,
            "source path is not part of the definition"
        );

        let mut changed = base.clone();
        changed.model = Some("provider/other".into());
        assert_ne!(agent_definition_digest(&changed), digest);

        let mut changed = base.clone();
        changed.inherit_global_context = true;
        assert_ne!(agent_definition_digest(&changed), digest);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn record_roundtrip_and_legacy_refusal() {
        let run_id = unique("recovery-roundtrip");
        start_record(&run_id, "single", Some("session-1")).unwrap();
        let record = read_record(&run_id).unwrap().expect("record");
        assert_eq!(record.parent_session_id.as_deref(), Some("session-1"));
        assert_eq!(record.state, "running");

        let dir = std::env::temp_dir().join(unique("rpi-recovery"));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("worker.md");
        std::fs::write(&path, "---\nname: worker\n---\nbody\n").unwrap();
        let agent = agent(path.clone());
        record_child(
            &run_id,
            RecoveryChild {
                index: 0,
                agent: agent.name.clone(),
                agent_path: path.to_string_lossy().to_string(),
                definition_digest: agent_definition_digest(&agent),
                cwd: dir.to_string_lossy().to_string(),
                session_file: None,
                launcher: None,
                launcher_recorded: true,
                state: "running".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let record = read_record(&run_id).unwrap().unwrap();
        assert_eq!(record.children.len(), 1);
        assert!(record.children[0].launcher_recorded);
        assert!(record.children[0].launcher.is_none());
        finish_record(&run_id, "complete");
        assert_eq!(read_record(&run_id).unwrap().unwrap().state, "complete");

        // Legacy run: no record.
        let missing = unique("recovery-missing");
        let error =
            trusted_resume_child(&missing, None, std::slice::from_ref(&agent), 0).unwrap_err();
        assert!(error.contains("no trusted recovery record"), "{error}");
        assert!(error.contains("reuseFrom"), "{error}");

        // Owner mismatch.
        let error = trusted_resume_child(
            &run_id,
            Some("other-session"),
            std::slice::from_ref(&agent),
            0,
        )
        .unwrap_err();
        assert!(error.contains("not found in the active session"), "{error}");

        // Digest mismatch.
        let mut changed = agent.clone();
        changed.model = Some("provider/other".into());
        let error = trusted_resume_child(&run_id, Some("session-1"), &[changed], 0).unwrap_err();
        assert!(error.contains("definition changed"), "{error}");

        // Success.
        let (record, child) =
            trusted_resume_child(&run_id, Some("session-1"), &[agent], 0).unwrap();
        assert_eq!(record.run_id, run_id);
        assert_eq!(child.index, 0);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(launch_dir(&run_id));
    }

    #[test]
    fn require_launcher_shape_and_checks() {
        let dir = std::env::temp_dir().join(unique("rpi-launcher"));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("worker.md");
        std::fs::write(&path, "---\nname: worker\n---\nbody\n").unwrap();
        let agent = agent(path.clone());
        let digest = agent_definition_digest(&agent);

        let mut params = serde_json::Map::new();
        params.insert("agent".into(), json!("worker"));
        params.insert("task".into(), json!("t"));
        params.insert("requireLauncher".into(), json!({"name": "sandbox"}));
        assert!(validate_require_launcher_shape(&params).is_ok());
        params.insert("tasks".into(), json!([]));
        assert!(validate_require_launcher_shape(&params).is_err());

        let required = RequiredLauncher {
            name: "sandbox".into(),
            definition_digest: digest.clone(),
        };
        let launcher = LauncherRecord {
            name: "sandbox".into(),
            definition_digest: digest,
        };
        assert!(check_required_launcher(&agent, Some(&launcher), &required).is_ok());
        let wrong_name = RequiredLauncher {
            name: "other".into(),
            ..required.clone()
        };
        assert!(check_required_launcher(&agent, Some(&launcher), &wrong_name).is_err());
        assert!(check_required_launcher(&agent, None, &required).is_err());

        // Missing launcher config fails closed; an explicit unwrapped record
        // returns None even when the agent file gained a launcher.
        let config = ExtensionConfig::new();
        let mut with_launcher = agent.clone();
        with_launcher.launcher = Some("sandbox".into());
        assert!(resolve_launcher(&config, &with_launcher, None, false).is_err());
        assert!(
            resolve_launcher(&config, &with_launcher, None, true)
                .unwrap()
                .is_none()
        );

        let mut config = ExtensionConfig::new();
        config.runner_launchers = Some(std::collections::BTreeMap::from([(
            "sandbox".to_string(),
            vec!["/usr/bin/sandbox".to_string(), "--flag".to_string()],
        )]));
        let resolved = resolve_launcher(&config, &with_launcher, None, false)
            .unwrap()
            .expect("launcher");
        assert_eq!(resolved.0.name, "sandbox");
        assert_eq!(resolved.1, vec!["/usr/bin/sandbox", "--flag"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
