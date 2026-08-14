//! Child argv/env assembly — the launch contract (FR-P0-05).
//!
//! Port of pi-subagents `src/runs/shared/pi-args.ts` `buildPiArgs` +
//! `resolvePiLaunchToolPlan` + `applyThinkingSuffix` @ v0.48.0 (56f97234),
//! P0 subset. Every branch mirrors the upstream order; golden-parity tests
//! drive both implementations from the same fixtures (TE04 G3).
//!
//! Intentional differences (all registered as TE04 deviations):
//! - env names `PI_SUBAGENT_*` → `RPI_SUBAGENT_*`; temp prefix
//!   `pi-subagent-` → `rpi-subagent-` (ADR-0001).
//! - upstream's always-injected `PROMPT_RUNTIME_EXTENSION_PATH` /
//!   `FANOUT_CHILD_EXTENSION_PATH` source-file extensions become this cdylib
//!   itself (`resolve_self_extension_path`), and boundary instructions are
//!   prepended parent-side into the system-prompt temp file instead of via a
//!   child-side runtime extension event.
//! - P1/P2-only inputs (structured output, permissions, watchdog, steering,
//!   tool budgets, capability ceilings, intercom session names, MCP direct
//!   selection) are not part of the input struct; their env vars are either
//!   unset or cleared exactly where upstream clears them for the inactive
//!   feature.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::agents::discover::{AgentConfig, ThinkingSpec};
use crate::paths;

pub const TASK_ARG_LIMIT: usize = 8000;
pub const SUBAGENT_TASK_DELIVERY_ENV: &str = "RPI_SUBAGENT_TASK_DELIVERY";
pub const SUBAGENT_CHILD_ENV: &str = "RPI_SUBAGENT_CHILD";
pub const SUBAGENT_FANOUT_CHILD_ENV: &str = "RPI_SUBAGENT_FANOUT_CHILD";
pub const SUBAGENT_INHERIT_PROJECT_CONTEXT_ENV: &str = "RPI_SUBAGENT_INHERIT_PROJECT_CONTEXT";
pub const SUBAGENT_INHERIT_SKILLS_ENV: &str = "RPI_SUBAGENT_INHERIT_SKILLS";
pub const SUBAGENT_PARENT_SESSION_ENV: &str = "RPI_SUBAGENT_PARENT_SESSION";
pub const SUBAGENT_RUN_ID_ENV: &str = "RPI_SUBAGENT_RUN_ID";
pub const SUBAGENT_CHILD_AGENT_ENV: &str = "RPI_SUBAGENT_CHILD_AGENT";
pub const SUBAGENT_CHILD_INDEX_ENV: &str = "RPI_SUBAGENT_CHILD_INDEX";
pub const SUBAGENT_PARENT_DEPTH_ENV: &str = "RPI_SUBAGENT_PARENT_DEPTH";
pub const SUBAGENT_PARENT_EVENT_SINK_ENV: &str = "RPI_SUBAGENT_PARENT_EVENT_SINK";
pub const SUBAGENT_STEER_INBOX_ENV: &str = "RPI_SUBAGENT_STEER_INBOX";
pub const SUBAGENT_PARENT_CONTROL_INBOX_ENV: &str = "RPI_SUBAGENT_PARENT_CONTROL_INBOX";
pub const SUBAGENT_PARENT_ROOT_RUN_ID_ENV: &str = "RPI_SUBAGENT_PARENT_ROOT_RUN_ID";
pub const SUBAGENT_PARENT_RUN_ID_ENV: &str = "RPI_SUBAGENT_PARENT_RUN_ID";
pub const SUBAGENT_PARENT_CHILD_INDEX_ENV: &str = "RPI_SUBAGENT_PARENT_CHILD_INDEX";
pub const SUBAGENT_PARENT_PATH_ENV: &str = "RPI_SUBAGENT_PARENT_PATH";
pub const SUBAGENT_PARENT_CAPABILITY_TOKEN_ENV: &str = "RPI_SUBAGENT_PARENT_CAPABILITY_TOKEN";
pub const SUBAGENT_ORCHESTRATOR_SESSION_ID_ENV: &str = "RPI_SUBAGENT_ORCHESTRATOR_SESSION_ID";
pub const REQUIRED_CHILD_TOOLS_ENV: &str = "RPI_SUBAGENT_REQUIRED_TOOLS";
pub const CHILD_TOOL_DIAGNOSTIC_PATH_ENV: &str = "RPI_SUBAGENT_TOOL_DIAGNOSTIC_PATH";

/// THINKING_LEVELS (model-info.ts:1).
pub const THINKING_LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskDelivery {
    Auto,
    File,
}

/// `resolveSubagentTaskDelivery` (pi-args.ts:78-84).
pub fn resolve_task_delivery() -> TaskDelivery {
    match std::env::var(SUBAGENT_TASK_DELIVERY_ENV) {
        Ok(value) if value.trim().to_lowercase() == "file" => TaskDelivery::File,
        _ => TaskDelivery::Auto,
    }
}

fn should_deliver_task_via_file(task: &str, delivery: TaskDelivery) -> bool {
    // `task.length` is UTF-16 code units upstream; encode_utf16 matches it.
    delivery == TaskDelivery::File || task.encode_utf16().count() > TASK_ARG_LIMIT
}

/// `escapeXmlAttr` (pi-args.ts:541-547).
fn escape_xml_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// `applyThinkingSuffix` (pi-args.ts:221-235). `replace_existing` is the
/// thinking-override path (fork sanitization sets `off` and replaces).
pub fn apply_thinking_suffix(
    model: Option<&str>,
    thinking: Option<&str>,
    replace_existing: bool,
) -> Option<String> {
    let model = model?;
    // `if (!model || !thinking) return model` — falsy thinking keeps the
    // model untouched (pi-args.ts:222).
    let Some(thinking) = thinking.filter(|t| !t.is_empty()) else {
        return Some(model.to_string());
    };
    if model.is_empty() {
        return Some(model.to_string());
    }
    if let Some(colon_idx) = model.rfind(':') {
        let suffix = &model[colon_idx + 1..];
        if THINKING_LEVELS.contains(&suffix) {
            return Some(if replace_existing {
                format!("{}:{thinking}", &model[..colon_idx])
            } else {
                model.to_string()
            });
        }
    }
    Some(format!("{model}:{thinking}"))
}

/// Boundary instruction blocks prepended to the child system prompt.
/// Upstream injects these from the always-loaded runtime extension
/// (`subagent-prompt-runtime.ts:42-57`); rpi prepends parent-side at temp-file
/// write time (same effective child prompt; TE04 deviation TE-D17).
pub const CHILD_SUBAGENT_BOUNDARY_INSTRUCTIONS: &str = "You are a child subagent, not the parent orchestrator.\nThe parent session owns delegation, orchestration, review fanout, and follow-up worker launches.\nIgnore prior parent-only orchestration instructions in inherited conversation history.\nDo not propose or run subagents. Complete only your assigned role-specific task with the tools available to you.\nIf you need to edit files, use the available editing tools. Do not print tool-call syntax, patches, or pseudo-tool calls as text.";

pub const CHILD_FANOUT_BOUNDARY_INSTRUCTIONS: &str = "You are a child subagent with explicit fanout responsibility for this assigned task.\nThe parent session owns final orchestration, acceptance, and follow-up implementation launches.\nYou may use the `subagent` tool only for the fanout work explicitly requested in this task.\nDo not broaden yourself into general parent orchestration. Do not launch follow-up workers unless the task explicitly asks for that.\nThe maxSubagentDepth cap still applies and may block further fanout.\nIf you need to edit files, use the available editing tools. Do not print tool-call syntax, patches, or pseudo-tool calls as text.";

/// P0 launch inputs (`BuildPiArgsInput` subset). Paths are already resolved by
/// the caller (session file/dir, artifacts).
#[derive(Debug, Clone, Default)]
pub struct BuildArgsInput {
    pub base_args: Vec<String>,
    pub task: String,
    pub task_delivery: Option<TaskDelivery>,
    pub session_enabled: bool,
    pub session_dir: Option<PathBuf>,
    pub session_file: Option<PathBuf>,
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub system_prompt: Option<String>,
    pub system_prompt_mode: &'static str,
    pub inherit_project_context: bool,
    pub inherit_skills: bool,
    pub require_read_tool: bool,
    pub tools: Option<Vec<String>>,
    pub extensions: Option<Vec<String>>,
    pub subagent_only_extensions: Option<Vec<String>>,
    pub mcp_direct_tools: Vec<String>,
    pub prompt_file_stem: Option<String>,
    pub run_id: Option<String>,
    pub child_agent_name: Option<String>,
    pub child_index: Option<usize>,
    pub parent_session_id: Option<String>,
    /// Boundary block to prepend (None = fanout child gets the fanout variant).
    pub fanout_authorized: bool,
    /// This cdylib's path for child `--extension` injection (upstream's
    /// always-injected PROMPT_RUNTIME slot). `None` skips injection — used by
    /// tests and by the degraded path when the path cannot be resolved.
    pub self_extension: Option<String>,
    /// Steer inbox dir for this child (FR-P1-04 steer); `None` clears the
    /// env (foreground runs have no live steer channel through the plugin).
    pub steer_inbox: Option<PathBuf>,
    /// Supervisor channel dir (FR-P1-10): activates the child-side
    /// `contact_supervisor` tool; `None` clears the env.
    pub supervisor_channel: Option<PathBuf>,
}

#[derive(Debug, Clone, Default)]
pub struct LaunchToolPlan {
    pub explicit_tool_allowlist: bool,
    pub effective_tool_allowlist: Vec<String>,
    pub required_child_tools: Vec<String>,
    pub fanout_authorized: bool,
    pub disable_ambient_extensions: bool,
    pub extension_args: Vec<String>,
}

/// `resolvePiLaunchToolPlan` (pi-args.ts:374-538) P0 subset: no capability
/// ceiling, no MCP direct resolution (P2 — `mcp:` names parse but resolve to
/// nothing), no structured-output internal tool. `self_extension` is the
/// always-injected runtime extension slot (upstream PROMPT_RUNTIME path →
/// this cdylib).
pub fn resolve_launch_tool_plan(
    tools: Option<&Vec<String>>,
    extensions: Option<&Vec<String>>,
    subagent_only_extensions: Option<&Vec<String>>,
    require_read_tool: bool,
    self_extension: Option<&str>,
) -> LaunchToolPlan {
    // Path-shaped entries in `tools` (contain `/` or end with .ts/.js) are
    // extension paths, not builtin tool names (pi-args.ts:385-389, 408-414).
    let is_path_shaped =
        |tool: &str| tool.contains('/') || tool.ends_with(".ts") || tool.ends_with(".js");
    let requested_builtin: Vec<String> = tools
        .map(|list| {
            list.iter()
                .filter(|t| !is_path_shaped(t))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let tool_extension_paths: Vec<String> = tools
        .map(|list| list.iter().filter(|t| is_path_shaped(t)).cloned().collect())
        .unwrap_or_default();

    // declaredBuiltinTools (395-406): tools undefined → no constraint; explicit
    // list gets `read` prepended when skills need lazy loading and it's absent.
    let declared_builtin = match &tools {
        None => Vec::new(),
        Some(_) => {
            let needs_read = require_read_tool
                && !requested_builtin.is_empty()
                && !requested_builtin.iter().any(|t| t == "read");
            if needs_read {
                let mut with_read = vec!["read".to_string()];
                with_read.extend(requested_builtin.iter().cloned());
                with_read
            } else {
                requested_builtin.clone()
            }
        }
    };
    let fanout_authorized = declared_builtin.iter().any(|t| t == "subagent");
    let explicit_tool_allowlist = tools.is_some();
    let effective_tool_allowlist = dedup(&declared_builtin);
    let required_child_tools = if explicit_tool_allowlist {
        dedup(&declared_builtin)
    } else {
        Vec::new()
    };

    // Extensions (445-471): declared `extensions` (even empty) disables
    // ambient discovery and lists everything explicitly; otherwise user
    // extensions stay ambient while runtime + tool paths + subagent-only
    // extensions are still passed.
    let disable_ambient_extensions = extensions.is_some();
    let mut runtime: Vec<String> = Vec::new();
    if let Some(path) = self_extension {
        runtime.push(path.to_string());
    }
    let subagent_only = subagent_only_extensions.cloned().unwrap_or_default();
    let extension_args = if disable_ambient_extensions {
        let configured: Vec<String> = tool_extension_paths
            .iter()
            .chain(extensions.unwrap().iter())
            .chain(subagent_only.iter())
            .cloned()
            .collect();
        dedup(
            &runtime
                .iter()
                .chain(configured.iter())
                .cloned()
                .collect::<Vec<_>>(),
        )
    } else {
        dedup(
            &runtime
                .iter()
                .chain(tool_extension_paths.iter())
                .chain(subagent_only.iter())
                .cloned()
                .collect::<Vec<_>>(),
        )
    };

    LaunchToolPlan {
        explicit_tool_allowlist,
        effective_tool_allowlist,
        required_child_tools,
        fanout_authorized,
        disable_ambient_extensions,
        extension_args,
    }
}

fn dedup(items: &[String]) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    items
        .iter()
        .filter(|item| seen.insert((*item).clone()))
        .cloned()
        .collect()
}

#[derive(Debug, Clone, Default)]
pub struct BuildArgsResult {
    pub args: Vec<String>,
    /// Env overlay; `None` values mean "clear for the child" (upstream sets
    /// the key to `undefined`, which removes it from the spawn env).
    pub env: BTreeMap<String, Option<String>>,
    pub temp_dir: PathBuf,
    pub tool_diagnostic_path: Option<PathBuf>,
}

fn mkdtemp_temp_dir() -> std::io::Result<PathBuf> {
    // mkdtemp semantics with the `rpi-subagent-` prefix (upstream
    // `mkdtempSync(join(os.tmpdir(), "pi-subagent-"))`).
    let base = paths::temp_dir().join("rpi-subagent-");
    let base = base.to_string_lossy().to_string();
    let mut attempts = 0;
    loop {
        let suffix = random_suffix();
        let candidate = format!("{base}{suffix}");
        match std::fs::create_dir(&candidate) {
            Ok(()) => return Ok(PathBuf::from(candidate)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && attempts < 32 => {
                attempts += 1;
            }
            Err(error) => return Err(error),
        }
    }
}

fn random_suffix() -> String {
    // 12 hex chars from /dev/urandom; time+pid fallback (mkdtemp uniqueness
    // only — not a security boundary).
    let mut bytes = [0u8; 6];
    if let Ok(mut source) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        if source.read_exact(&mut bytes).is_ok() {
            return bytes.iter().map(|b| format!("{b:02x}")).collect();
        }
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:x}{:x}", nanos, std::process::id())
}

/// `buildPiArgs` (pi-args.ts:549-830), P0 subset. Performs the same directory
/// creation (`--session`/`--session-dir` mkdir recursive) and temp-file writes
/// (0600 prompt/task) as the upstream function.
pub fn build_rpi_args(input: &BuildArgsInput) -> crate::error::Result<BuildArgsResult> {
    let mut args = input.base_args.clone();

    // --- session (552-563) ---
    if let Some(session_file) = &input.session_file {
        if let Some(parent) = session_file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        args.push("--session".into());
        args.push(session_file.to_string_lossy().to_string());
    } else {
        if !input.session_enabled {
            args.push("--no-session".into());
        }
        if let Some(session_dir) = &input.session_dir {
            std::fs::create_dir_all(session_dir)?;
            args.push("--session-dir".into());
            args.push(session_dir.to_string_lossy().to_string());
        }
    }

    // --- model (565-568) ---
    let model_arg = apply_thinking_suffix(input.model.as_deref(), input.thinking.as_deref(), false);
    if let Some(model_arg) = &model_arg {
        args.push("--model".into());
        args.push(model_arg.clone());
    }

    // --- tools / extensions (570-595) ---
    let tool_plan = resolve_launch_tool_plan(
        input.tools.as_ref(),
        input.extensions.as_ref(),
        input.subagent_only_extensions.as_ref(),
        input.require_read_tool,
        input.self_extension.as_deref(),
    );
    if tool_plan.explicit_tool_allowlist {
        if !tool_plan.effective_tool_allowlist.is_empty() {
            args.push("--tools".into());
            args.push(tool_plan.effective_tool_allowlist.join(","));
        } else {
            args.push("--no-tools".into());
        }
    }
    if tool_plan.disable_ambient_extensions {
        args.push("--no-extensions".into());
    }
    for ext in &tool_plan.extension_args {
        args.push("--extension".into());
        args.push(ext.clone());
    }

    // --- inherit switches (597-602) ---
    if !input.inherit_project_context {
        args.push("--no-context-files".into());
    }
    if !input.inherit_skills {
        args.push("--no-skills".into());
    }

    // --- system prompt temp file (604-621) ---
    let mut temp_dir: Option<PathBuf> = None;
    if let Some(system_prompt) = &input.system_prompt {
        let dir = mkdtemp_temp_dir()?;
        let stem = input
            .prompt_file_stem
            .as_deref()
            .unwrap_or("prompt")
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
                    c
                } else {
                    '_'
                }
            })
            .collect::<String>();
        let prompt_path = dir.join(format!("{stem}.md"));
        let boundary = if input.fanout_authorized {
            CHILD_FANOUT_BOUNDARY_INSTRUCTIONS
        } else {
            CHILD_SUBAGENT_BOUNDARY_INSTRUCTIONS
        };
        let tagged_prompt = match &input.child_agent_name {
            // <active_agent> prefix (pi-args.ts:609-613) + boundary block
            // (parent-side injection, TE-D17).
            Some(name) => format!(
                "<active_agent name=\"{}\"/>\n\n{}\n\n{}",
                escape_xml_attr(name),
                boundary,
                system_prompt
            ),
            None => format!("{boundary}\n\n{system_prompt}"),
        };
        write_private_file(&prompt_path, &tagged_prompt)?;
        args.push(
            if input.system_prompt_mode == "replace" {
                "--system-prompt"
            } else {
                "--append-system-prompt"
            }
            .into(),
        );
        args.push(prompt_path.to_string_lossy().to_string());
        temp_dir = Some(dir);
    }

    // --- task delivery (623-637) ---
    let delivery = input.task_delivery.unwrap_or_else(resolve_task_delivery);
    if should_deliver_task_via_file(&input.task, delivery) {
        if temp_dir.is_none() {
            temp_dir = Some(mkdtemp_temp_dir()?);
        }
        let dir = temp_dir.clone().unwrap_or_default();
        let task_path = dir.join("task.md");
        write_private_file(&task_path, &format!("Task: {}", input.task))?;
        args.push(format!("@{}", task_path.to_string_lossy()));
    } else {
        args.push(format!("Task: {}", input.task));
    }

    // --- env (639-820) ---
    let mut env: BTreeMap<String, Option<String>> = BTreeMap::new();
    let temp_dir = match temp_dir {
        Some(dir) => dir,
        None => mkdtemp_temp_dir()?,
    };
    let mut tool_diagnostic_path: Option<PathBuf> = None;
    if !tool_plan.required_child_tools.is_empty() {
        let path = temp_dir.join("tool-diagnostic.json");
        env.insert(
            REQUIRED_CHILD_TOOLS_ENV.into(),
            Some(serde_json::to_string(&tool_plan.required_child_tools).unwrap_or_default()),
        );
        env.insert(
            CHILD_TOOL_DIAGNOSTIC_PATH_ENV.into(),
            Some(path.to_string_lossy().to_string()),
        );
        tool_diagnostic_path = Some(path);
    }
    // MCP direct tools are P2: always the upstream "__none__" sentinel.
    env.insert("MCP_DIRECT_TOOLS".into(), Some("__none__".into()));
    let _ = &input.mcp_direct_tools;
    env.insert(SUBAGENT_CHILD_ENV.into(), Some("1".into()));
    env.insert(
        SUBAGENT_FANOUT_CHILD_ENV.into(),
        Some(
            if tool_plan.fanout_authorized {
                "1"
            } else {
                "0"
            }
            .into(),
        ),
    );
    // Nested-route vars (pi-args.ts:672-736): authorized children carry the
    // real routing values (no sink/inbox inputs in P0, so those stay ""), the
    // rest carry the cleared value upstream uses for unauthorized children.
    let parent_run_id = input.run_id.clone().unwrap_or_default();
    let parent_child_index = input
        .child_index
        .map(|index| index.to_string())
        .unwrap_or_default();
    // parentPath (pi-args.ts:690-703): one entry for this run; stepIndex
    // only when the child index is numeric.
    let mut parent_path_entry = serde_json::Map::new();
    parent_path_entry.insert("runId".into(), Value::String(parent_run_id.clone()));
    if let Some(index) = input.child_index {
        parent_path_entry.insert("stepIndex".into(), Value::from(index as u64));
    }
    if let Some(agent) = &input.child_agent_name {
        parent_path_entry.insert("agent".into(), Value::String(agent.clone()));
    }
    let cleared = |env: &mut BTreeMap<String, Option<String>>, key: &str| {
        env.insert(key.to_string(), Some(String::new()));
    };
    cleared(&mut env, SUBAGENT_PARENT_EVENT_SINK_ENV);
    cleared(&mut env, SUBAGENT_PARENT_CONTROL_INBOX_ENV);
    if let Some(inbox) = &input.steer_inbox {
        env.insert(
            SUBAGENT_STEER_INBOX_ENV.to_string(),
            Some(inbox.to_string_lossy().to_string()),
        );
    } else {
        cleared(&mut env, SUBAGENT_STEER_INBOX_ENV);
    }
    if let Some(channel) = &input.supervisor_channel {
        env.insert(
            crate::p1::supervisor::SUPERVISOR_CHANNEL_DIR_ENV.to_string(),
            Some(channel.to_string_lossy().to_string()),
        );
    } else {
        cleared(&mut env, crate::p1::supervisor::SUPERVISOR_CHANNEL_DIR_ENV);
    }
    cleared(&mut env, SUBAGENT_PARENT_CAPABILITY_TOKEN_ENV);
    env.insert(
        SUBAGENT_PARENT_ROOT_RUN_ID_ENV.into(),
        Some(if tool_plan.fanout_authorized {
            input.run_id.clone().unwrap_or_default()
        } else {
            String::new()
        }),
    );
    env.insert(
        SUBAGENT_PARENT_RUN_ID_ENV.into(),
        Some(if tool_plan.fanout_authorized {
            parent_run_id.clone()
        } else {
            String::new()
        }),
    );
    env.insert(
        SUBAGENT_PARENT_CHILD_INDEX_ENV.into(),
        Some(if tool_plan.fanout_authorized {
            parent_child_index.clone()
        } else {
            String::new()
        }),
    );
    env.insert(
        SUBAGENT_PARENT_DEPTH_ENV.into(),
        Some(if tool_plan.fanout_authorized {
            "1".to_string()
        } else {
            String::new()
        }),
    );
    env.insert(
        SUBAGENT_PARENT_PATH_ENV.into(),
        Some(if tool_plan.fanout_authorized {
            serde_json::to_string(&vec![parent_path_entry]).unwrap_or_default()
        } else {
            String::new()
        }),
    );
    // pi-args.ts:752-754: parentSessionId also rides along for intercom
    // routing.
    if let Some(parent_session_id) = &input.parent_session_id {
        env.insert(
            SUBAGENT_ORCHESTRATOR_SESSION_ID_ENV.into(),
            Some(parent_session_id.clone()),
        );
    }
    env.insert(
        SUBAGENT_INHERIT_PROJECT_CONTEXT_ENV.into(),
        Some(
            if input.inherit_project_context {
                "1"
            } else {
                "0"
            }
            .into(),
        ),
    );
    env.insert(
        SUBAGENT_INHERIT_SKILLS_ENV.into(),
        Some(if input.inherit_skills { "1" } else { "0" }.into()),
    );
    if let Some(run_id) = &input.run_id {
        env.insert(SUBAGENT_RUN_ID_ENV.into(), Some(run_id.clone()));
    }
    if let Some(name) = &input.child_agent_name {
        env.insert(SUBAGENT_CHILD_AGENT_ENV.into(), Some(name.clone()));
    }
    if let Some(index) = input.child_index {
        env.insert(SUBAGENT_CHILD_INDEX_ENV.into(), Some(index.to_string()));
    }
    // Parent session: explicit value, else inherited, else "" (819-820).
    let parent_session = input
        .parent_session_id
        .clone()
        .or_else(|| std::env::var(SUBAGENT_PARENT_SESSION_ENV).ok())
        .unwrap_or_default();
    env.insert(SUBAGENT_PARENT_SESSION_ENV.into(), Some(parent_session));

    Ok(BuildArgsResult {
        args,
        env,
        temp_dir,
        tool_diagnostic_path,
    })
}

/// Write a 0o600 file (upstream `fs.writeFileSync(path, data, { mode: 0o600 })`).
pub fn write_private_file(path: &Path, content: &str) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(content.as_bytes())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, content)
    }
}

/// `cleanupTempDir` best-effort removal (upstream unlinks the mkdtemp tree).
pub fn cleanup_temp_dir(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

/// Effective thinking for a launch: fork override (P1) > agent frontmatter;
/// `false` (Disabled) means "no suffix" (agents.ts:1629 semantics).
pub fn effective_thinking(agent: &AgentConfig, thinking_override: Option<&str>) -> Option<String> {
    if let Some(override_level) = thinking_override {
        return Some(override_level.to_string());
    }
    match &agent.thinking {
        ThinkingSpec::Level(level) => Some(level.clone()),
        ThinkingSpec::Disabled | ThinkingSpec::Unset => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_input() -> BuildArgsInput {
        BuildArgsInput {
            base_args: vec!["--mode".into(), "json".into(), "-p".into()],
            task: "do the thing".into(),
            session_enabled: true,
            system_prompt: None,
            system_prompt_mode: "replace",
            inherit_project_context: true,
            inherit_skills: false,
            tools: None,
            ..Default::default()
        }
    }

    #[test]
    fn minimal_args_shape() {
        let dir = std::env::temp_dir().join(format!("rpi-sub-args-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut input = base_input();
        input.session_dir = Some(dir.join("run-0"));
        let result = build_rpi_args(&input).unwrap();
        assert_eq!(
            result.args,
            vec![
                "--mode",
                "json",
                "-p",
                "--session-dir",
                input
                    .session_dir
                    .clone()
                    .unwrap()
                    .to_string_lossy()
                    .as_ref(),
                "--no-skills",
                "Task: do the thing",
            ]
        );
        assert_eq!(result.env.get(SUBAGENT_CHILD_ENV), Some(&Some("1".into())));
        assert_eq!(
            result.env.get(SUBAGENT_FANOUT_CHILD_ENV),
            Some(&Some("0".into()))
        );
        assert!(result.tool_diagnostic_path.is_none());
        cleanup_temp_dir(&result.temp_dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tools_three_branches() {
        // omitted: no --tools flag
        let plan = resolve_launch_tool_plan(None, None, None, false, None);
        assert!(!plan.explicit_tool_allowlist);
        // empty list: --no-tools
        let plan = resolve_launch_tool_plan(Some(&Vec::new()), None, None, false, None);
        assert!(plan.explicit_tool_allowlist);
        assert!(plan.effective_tool_allowlist.is_empty());
        // values: --tools with read auto-added when skills require it
        let tools = vec!["bash".to_string(), "write".to_string()];
        let plan = resolve_launch_tool_plan(Some(&tools), None, None, true, None);
        assert_eq!(plan.effective_tool_allowlist, vec!["read", "bash", "write"]);
        assert_eq!(plan.required_child_tools, vec!["read", "bash", "write"]);
        assert!(!plan.fanout_authorized);
        // subagent in tools authorizes fanout
        let tools = vec!["read".to_string(), "subagent".to_string()];
        let plan = resolve_launch_tool_plan(Some(&tools), None, None, false, None);
        assert!(plan.fanout_authorized);
    }

    #[test]
    fn extensions_semantics() {
        const SELF: &str = "/plugins/librpi_ext_subagents.so";
        // present (even empty) disables ambient + lists runtime first
        let plan = resolve_launch_tool_plan(
            Some(&vec!["read".into()]),
            Some(&vec![]),
            None,
            false,
            Some(SELF),
        );
        assert!(plan.disable_ambient_extensions);
        assert_eq!(plan.extension_args, vec![SELF]);
        // declared extensions are listed after the runtime extension
        let plan = resolve_launch_tool_plan(
            None,
            Some(&vec!["/ext/other.so".into()]),
            None,
            false,
            Some(SELF),
        );
        assert!(plan.disable_ambient_extensions);
        assert_eq!(plan.extension_args, vec![SELF, "/ext/other.so"]);
        // omitted: ambient stays; tool-shaped + subagent-only still listed
        let plan = resolve_launch_tool_plan(
            None,
            None,
            Some(&vec!["/path/to/ext.so".to_string()]),
            false,
            Some(SELF),
        );
        assert!(!plan.disable_ambient_extensions);
        assert_eq!(plan.extension_args, vec![SELF, "/path/to/ext.so"]);
        // tool entries with path shapes route to extensions, not --tools
        let tools = vec!["read".to_string(), "/abs/tool.so".to_string()];
        let plan = resolve_launch_tool_plan(Some(&tools), None, None, false, Some(SELF));
        assert_eq!(plan.effective_tool_allowlist, vec!["read"]);
        assert_eq!(plan.extension_args, vec![SELF, "/abs/tool.so"]);
    }

    #[test]
    fn thinking_suffix_rules() {
        assert_eq!(
            apply_thinking_suffix(Some("provider/m"), Some("high"), false),
            Some("provider/m:high".into())
        );
        assert_eq!(
            apply_thinking_suffix(Some("provider/m:low"), Some("high"), false),
            Some("provider/m:low".into())
        );
        assert_eq!(
            apply_thinking_suffix(Some("provider/m:low"), Some("high"), true),
            Some("provider/m:high".into())
        );
        assert_eq!(apply_thinking_suffix(None, Some("high"), false), None);
        assert_eq!(
            apply_thinking_suffix(Some("m"), None, false),
            Some("m".into())
        );
    }

    #[test]
    fn long_task_goes_to_file_and_env_carry_required_tools() {
        let dir = std::env::temp_dir().join(format!("rpi-sub-args-long-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let long_task = "x".repeat(8001);
        let mut input = base_input();
        input.task = long_task.clone();
        input.tools = Some(vec!["read".to_string()]);
        input.session_dir = Some(dir.join("run-0"));
        input.system_prompt = Some("be brief".into());
        input.prompt_file_stem = Some("scout".into());
        input.child_agent_name = Some("scout".into());
        input.run_id = Some("abcd1234".into());
        input.child_index = Some(0);
        let result = build_rpi_args(&input).unwrap();
        let last = result.args.last().unwrap();
        assert!(last.starts_with('@'), "{last}");
        let prompt_flag = result
            .args
            .iter()
            .position(|a| a == "--system-prompt")
            .expect("replace mode uses --system-prompt");
        let prompt_path = PathBuf::from(&result.args[prompt_flag + 1]);
        let content = std::fs::read_to_string(&prompt_path).unwrap();
        assert!(content.starts_with("<active_agent name=\"scout\"/>\n\n"));
        assert!(content.contains("You are a child subagent, not the parent orchestrator."));
        assert!(content.ends_with("be brief"));
        let mode = std::fs::metadata(&prompt_path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(mode.permissions().mode() & 0o777, 0o600);
        }
        assert_eq!(
            result.env.get(REQUIRED_CHILD_TOOLS_ENV),
            Some(&Some("[\"read\"]".into()))
        );
        assert!(result.tool_diagnostic_path.is_some());
        assert_eq!(
            result.env.get(SUBAGENT_CHILD_AGENT_ENV),
            Some(&Some("scout".into()))
        );
        cleanup_temp_dir(&result.temp_dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_delivery_env_forces_file() {
        // Delivered via explicit input rather than env mutation (tests share
        // the process env).
        let dir = std::env::temp_dir().join(format!("rpi-sub-args-del-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut input = base_input();
        input.task_delivery = Some(TaskDelivery::File);
        input.session_dir = Some(dir.join("run-0"));
        let result = build_rpi_args(&input).unwrap();
        assert!(result.args.last().unwrap().starts_with('@'));
        cleanup_temp_dir(&result.temp_dir);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
