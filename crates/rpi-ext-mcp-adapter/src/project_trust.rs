//! Project-level MCP server trust and approval (port of
//! `project-server-trust.ts` @ pi-mcp-adapter v4.0.0 `5884ac4e`, #681
//! `5d645df` + #709 `1f540b9`, driven by rpi's [VARIANT] `mcp.json` layout
//! instead of `mcp-adapter.json`, TE-D44).
//!
//! v3.0 semantics: a server defined by a PROJECT-scope config source
//! (`<cwd>/.mcp.json` / `<cwd>/.rpi/mcp.json`) may run local commands or
//! make network requests with the user's permissions, so it is gated on
//! (a) the host's project trust and (b) a per-server approval record keyed
//! by (canonical project scope, name, definition hash). Untrusted projects
//! and non-interactive sessions block; an interactive trusted session asks
//! once and remembers the answer. Approvals are shared across git
//! worktrees of the same repository (#709).
//!
//! The runtime-owner half of the upstream file (`excludeProjectServersAtLoadTime`)
//! is [`exclude_project_servers_at_load_time`]; the session gate result is
//! handed to [`crate::proxy::initialize_mcp`] through [`set_session_gate`].

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde_json::{Value, json};
use sha2::Digest;
use tracing::warn;

use crate::config::{LoadedMcpConfig, ProjectServerPolicy};
use crate::metadata::{McpConfig, ServerEntry};

const APPROVALS_VERSION: u64 = 1;
const APPROVALS_FILE: &str = "mcp-project-approvals.json";

/// `ProjectServerBlockReason` (types.ts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectServerBlockReason {
    Untrusted,
    ApprovalRequired,
    Denied,
}

impl ProjectServerBlockReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Untrusted => "untrusted",
            Self::ApprovalRequired => "approval-required",
            Self::Denied => "denied",
        }
    }
}

/// `ProjectServerBlock` (types.ts): the reason plus the defining source path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectServerBlock {
    pub reason: ProjectServerBlockReason,
    pub source: String,
}

/// `describeProjectServerBlock` (project-server-trust.ts:30-40).
pub fn describe_project_server_block(reason: ProjectServerBlockReason) -> &'static str {
    match reason {
        ProjectServerBlockReason::Untrusted => {
            "blocked by project trust — trust the project to review and approve this server"
        }
        ProjectServerBlockReason::ApprovalRequired => {
            "blocked: project server approval required — approve it in a trusted interactive session or set user-global settings.projectServers to \"allow\""
        }
        ProjectServerBlockReason::Denied => {
            "blocked: project server approval denied — reload in a trusted interactive session to approve it"
        }
    }
}

/// `disabledServerReason` (project-server-trust.ts:42-45): the generic
/// disabled text, or the block reason when this server was blocked by the
/// project gate.
pub fn disabled_server_reason(
    blocked: Option<&std::collections::HashMap<String, ProjectServerBlock>>,
    name: &str,
) -> String {
    match blocked.and_then(|map| map.get(name)) {
        Some(block) => describe_project_server_block(block.reason).to_string(),
        None => format!("disabled. Run /mcp enable {name} and /reload to enable it."),
    }
}

/// `hashProjectServerDefinition`: sha256 over the canonicalized JSON
/// (recursively sorted object keys; JS `undefined` members have no JSON
/// representation, so only present fields participate).
pub fn hash_project_server_definition(definition: &ServerEntry) -> String {
    let canonical = canonicalize(&Value::Object(definition.as_map().clone()));
    let rendered = serde_json::to_string(&canonical).unwrap_or_default();
    let digest = sha2::Sha256::digest(rendered.as_bytes());
    let mut hex = String::with_capacity(64);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

fn canonicalize(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(canonicalize).collect()),
        Value::Object(map) => {
            let mut sorted: Vec<(&String, &Value)> = map.iter().collect();
            sorted.sort_by(|a, b| a.0.cmp(b.0));
            Value::Object(
                sorted
                    .into_iter()
                    .map(|(key, value)| (key.clone(), canonicalize(value)))
                    .collect(),
            )
        }
        other => other.clone(),
    }
}

/// `canonicalProjectRoot`: realpath when resolvable, else the lexical
/// absolute path.
pub fn canonical_project_root(cwd: &str) -> String {
    let path = Path::new(cwd);
    match std::fs::canonicalize(path) {
        Ok(resolved) => resolved.to_string_lossy().into_owned(),
        Err(_) => {
            let absolute = if path.is_absolute() {
                path.to_path_buf()
            } else {
                std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("/"))
                    .join(path)
            };
            absolute.to_string_lossy().into_owned()
        }
    }
}

/// `projectApprovalScope` (#709, 1f540b9; project-server-trust.ts:80-124):
/// git worktrees share approvals per repository, keyed by the same relative
/// path. A `.git` file counts only when it is a regular file and git's admin
/// entry links back to it; bare repositories and `--separate-git-dir`
/// checkouts get a `git-dir:` key that no canonical path equals.
pub fn project_approval_scope(cwd: &str) -> String {
    let root = canonical_project_root(cwd);
    let mut dir = PathBuf::from(&root);
    loop {
        let dot_git = dir.join(".git");
        if dot_git.exists() {
            let relative = Path::new(&root)
                .strip_prefix(&dir)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            return match linked_worktree_repo_scope(&dot_git) {
                Some(repo_scope) if relative.is_empty() => repo_scope,
                Some(repo_scope) => join_scope(&repo_scope, &relative),
                None => root,
            };
        }
        match dir.parent() {
            Some(parent) if parent != dir => dir = parent.to_path_buf(),
            _ => return root,
        }
    }
}

/// `path.resolve(base, value)`: absolute values pass through; relative
/// values join onto `base` (upstream Node path resolution for git's admin
/// back-link, project-server-trust.ts:115).
fn resolve_from(base: &Path, value: &str) -> PathBuf {
    let candidate = Path::new(value);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        base.join(candidate)
    }
}

fn join_scope(scope: &str, relative: &str) -> String {
    Path::new(scope)
        .join(relative)
        .to_string_lossy()
        .into_owned()
}

fn linked_worktree_repo_scope(dot_git: &Path) -> Option<String> {
    // Upstream `lstatSync(dotGit).isFile()`: a SYMLINK must not count as the
    // `.git` file — otherwise a planted symlink could borrow another
    // checkout's approvals (project-server-trust.ts:110 @ 5884ac4e).
    if !std::fs::symlink_metadata(dot_git).ok()?.is_file() {
        return None;
    }
    let pointer = std::fs::read_to_string(dot_git).ok()?;
    let pointer = pointer
        .lines()
        .find_map(|line| line.strip_prefix("gitdir: "))?
        .trim()
        .to_string();
    let base = dot_git.parent()?;
    let admin_dir = std::fs::canonicalize(base.join(pointer)).ok()?;
    let back_link = std::fs::read_to_string(admin_dir.join("gitdir")).ok()?;
    // Upstream resolves the back-link against the ADMIN dir:
    // `resolve(adminDir, backLink)` (project-server-trust.ts:115).
    let back_link = std::fs::canonicalize(resolve_from(&admin_dir, back_link.trim())).ok()?;
    if back_link != std::fs::canonicalize(dot_git).ok()? {
        return None;
    }
    let common_dir = admin_dir.parent()?.parent()?.to_path_buf();
    // Upstream reads `<commonDir>/config` and a failure aborts the scope
    // resolution (the caller falls back to the checkout root). Parse
    // case-insensitively via `eq_ignore_ascii_case` (the upstream regex has
    // the `i` flag, so `BARE = true` counts too).
    let config = std::fs::read_to_string(common_dir.join("config")).ok()?;
    let bare = config.lines().any(|line| {
        let trimmed = line.trim();
        match trimmed.split_once('=') {
            Some((key, value)) if key.trim().eq_ignore_ascii_case("bare") => {
                matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "true" | "yes" | "on" | "1"
                )
            }
            _ => false,
        }
    });
    if common_dir.file_name().and_then(|name| name.to_str()) == Some(".git") && !bare {
        common_dir
            .parent()
            .map(|parent| parent.to_string_lossy().into_owned())
    } else {
        Some(format!("git-dir:{}", common_dir.to_string_lossy()))
    }
}

/// One persisted approval (`ApprovalRecord`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRecord {
    pub project_root: String,
    pub server_name: String,
    pub definition_hash: String,
    pub approved_at: String,
}

/// `approvalPath`: `<agent dir>/mcp-project-approvals.json`.
pub fn approval_path() -> PathBuf {
    crate::config::get_agent_dir().join(APPROVALS_FILE)
}

fn load_approvals(path: &Path) -> Vec<ApprovalRecord> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(parsed) = serde_json::from_str::<Value>(&raw) else {
        warn!(path = %path.display(), "MCP: ignoring invalid project-server approval store");
        return Vec::new();
    };
    if parsed.get("version").and_then(Value::as_u64) != Some(APPROVALS_VERSION) {
        warn!(path = %path.display(), "MCP: ignoring invalid project-server approval store");
        return Vec::new();
    }
    parsed
        .get("approvals")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| {
                    Some(ApprovalRecord {
                        project_root: entry.get("projectRoot")?.as_str()?.to_string(),
                        server_name: entry.get("serverName")?.as_str()?.to_string(),
                        definition_hash: entry.get("definitionHash")?.as_str()?.to_string(),
                        approved_at: entry.get("approvedAt")?.as_str()?.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn save_approval(path: &Path, record: ApprovalRecord) {
    let mut approvals = load_approvals(path);
    approvals.retain(|entry| {
        !(entry.project_root == record.project_root && entry.server_name == record.server_name)
    });
    approvals.push(record);
    let payload = serde_json::to_string_pretty(&json!({
        "version": APPROVALS_VERSION,
        "approvals": approvals.iter().map(|entry| json!({
            "projectRoot": entry.project_root,
            "serverName": entry.server_name,
            "definitionHash": entry.definition_hash,
            "approvedAt": entry.approved_at,
        })).collect::<Vec<_>>(),
    }))
    .unwrap_or_else(|_| "{}".to_string());
    if let Some(parent) = path.parent() {
        #[cfg(unix)]
        let existed = parent.exists();
        let _ = std::fs::create_dir_all(parent);
        // Only tighten a directory this code created: forcing 0700 on every
        // write overrides an operator's deliberate mode (round-2).
        #[cfg(unix)]
        if !existed {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }
    let temp = path.with_extension(format!("{}-{}.tmp", std::process::id(), random_suffix()));
    // Write-then-rename is only crash-safe when the write succeeded
    // (v0.1.6 review P2-11): a failed or partial write must leave the
    // previous approval store in place, not rename a torn file over it.
    if write_temp_file(&temp, &payload).is_err() {
        let _ = std::fs::remove_file(&temp);
        return;
    }
    if std::fs::rename(&temp, path).is_err() {
        let _ = std::fs::remove_file(&temp);
    } else {
        sync_parent_dir(path);
    }
}

/// Best-effort parent-directory sync after the rename (the file data was
/// already flushed); unix-only because Windows cannot open a directory as a
/// file. A failure here cannot corrupt the store.
fn sync_parent_dir(path: &Path) {
    #[cfg(unix)]
    if let Some(parent) = path.parent()
        && let Ok(dir) = std::fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Create `temp` exclusively and write the full payload before the caller
/// renames it over the store; `sync_all` flushes it to disk first.
fn write_temp_file(temp: &Path, payload: &str) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(temp)?;
    use std::io::Write;
    file.write_all(payload.as_bytes())?;
    file.sync_all()
}

fn random_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}")
}

/// `approveProjectServer`.
pub fn approve_project_server(
    path: &Path,
    project_root: &str,
    server_name: &str,
    definition: &ServerEntry,
) {
    save_approval(
        path,
        ApprovalRecord {
            project_root: project_root.to_string(),
            server_name: server_name.to_string(),
            definition_hash: hash_project_server_definition(definition),
            approved_at: now_iso8601(),
        },
    );
}

fn now_iso8601() -> String {
    // Millisecond-precision UTC like `new Date().toISOString()`
    // (v0.1.6 review round 2: the field used to hold epoch seconds under an
    // ISO-8601 name; the reader accepts any string, so existing stores stay
    // readable).
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// `describeServer` (project-server-trust.ts:184-192): the confirmation
/// detail line.
pub fn describe_server(definition: &ServerEntry) -> String {
    if let Some(command) = definition.get_str("command") {
        let mut parts = vec![format!("{command:?}")];
        if let Some(Value::Array(args)) = definition.get("args") {
            parts.extend(
                args.iter()
                    .filter_map(Value::as_str)
                    .map(|arg| format!("{arg:?}")),
            );
        }
        return parts.join(" ");
    }
    if let Some(url) = definition.get_str("url") {
        return url.to_string();
    }
    if let Some(socket) = definition.get_str("socket") {
        return socket.to_string();
    }
    "(no command or endpoint)".to_string()
}

/// Outcome of the trust gate (`ProjectTrustResult`).
pub struct TrustResult {
    pub config: McpConfig,
    pub blocked: std::collections::HashMap<String, ProjectServerBlock>,
}

/// `applyProjectServerTrust` (project-server-trust.ts:194-238), synchronous:
/// the host's `ui.confirm` blocks the calling thread, so `confirm` is a
/// plain closure over the host channel.
pub fn apply_project_server_trust<F>(
    loaded: &LoadedMcpConfig,
    project_root: &str,
    project_trusted: bool,
    has_ui: bool,
    mut confirm: F,
) -> TrustResult
where
    F: FnMut(&str, &str) -> bool,
{
    let mut config = loaded.config.clone();
    let mut blocked = std::collections::HashMap::new();
    if loaded.project_servers.is_empty() {
        return TrustResult { config, blocked };
    }
    let approvals = load_approvals(&approval_path());
    let approvals_path = approval_path();

    for (name, source) in &loaded.project_servers {
        let Some(definition) = config.mcp_servers.get(name).cloned() else {
            continue;
        };
        if definition.is_disabled() {
            continue;
        }
        let definition_hash = hash_project_server_definition(&definition);
        let approved = approvals.iter().any(|entry| {
            entry.project_root == project_root
                && entry.server_name == *name
                && entry.definition_hash == definition_hash
        });
        if project_trusted
            && (approved || (!has_ui && loaded.project_server_policy == ProjectServerPolicy::Allow))
        {
            continue;
        }

        let reason = if !project_trusted {
            ProjectServerBlockReason::Untrusted
        } else if !has_ui {
            ProjectServerBlockReason::ApprovalRequired
        } else {
            let title = format!("Allow project MCP server \u{201c}{name}\u{201d}?");
            let message = format!(
                "Project config: {}\nEndpoint: {}\n\nThis server can run local commands or make network requests with your user permissions.",
                source.path,
                describe_server(&definition)
            );
            if confirm(&title, &message) {
                approve_project_server(&approvals_path, project_root, name, &definition);
                continue;
            }
            ProjectServerBlockReason::Denied
        };
        let mut next = definition.as_map().clone();
        next.insert("disabled".to_string(), Value::Bool(true));
        config.mcp_servers.insert(name.clone(), ServerEntry(next));
        blocked.insert(
            name.clone(),
            ProjectServerBlock {
                reason,
                source: source.path.clone(),
            },
        );
    }
    TrustResult { config, blocked }
}

/// `excludeProjectServersAtLoadTime`: remove project-derived servers before
/// an ExtensionContext exists (the load-time config may not act on the
/// project's permissions at all).
pub fn exclude_project_servers_at_load_time(loaded: &LoadedMcpConfig) -> McpConfig {
    let mut config = loaded.config.clone();
    for name in loaded.project_servers.keys() {
        config.mcp_servers.shift_remove(name);
    }
    config
}

/// Session trust gates handed from the host-binding layer (`lib.rs`) to
/// [`crate::proxy::initialize_mcp`], keyed by session cwd so two init
/// attempts for different working directories (parallel integration tests,
/// sequential sessions) never consume each other's gate.
static SESSION_GATES: OnceLock<Mutex<std::collections::HashMap<std::path::PathBuf, TrustResult>>> =
    OnceLock::new();

fn session_gates() -> &'static Mutex<std::collections::HashMap<std::path::PathBuf, TrustResult>> {
    SESSION_GATES.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// Install the next init's trust result for `cwd` (called on
/// `session_start` before `start_init`).
pub fn set_session_gate(cwd: &Path, result: TrustResult) {
    session_gates()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(cwd.to_path_buf(), result);
}

/// Consume the gate for `cwd` (one init attempt owns it).
pub fn take_session_gate(cwd: &Path) -> Option<TrustResult> {
    session_gates()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(cwd)
}

/// The set of blocked project servers carried by the current runtime.
pub type BlockedServers = std::collections::HashMap<String, ProjectServerBlock>;

/// A convenience used by tests and callers: the names of project-scope
/// servers in a loaded config.
pub fn project_server_names(loaded: &LoadedMcpConfig) -> HashSet<String> {
    loaded.project_servers.keys().cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn server(value: Value) -> ServerEntry {
        ServerEntry(value.as_object().cloned().unwrap_or_default())
    }

    /// v0.1.6 review P2-11: a failed write or rename must leave the target
    /// untouched and clean up the temp file.
    #[test]
    fn failed_approval_write_cleans_up_and_leaves_the_target() {
        let sandbox = std::env::temp_dir().join(format!(
            "rpi-mcp-trust-write-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&sandbox).expect("sandbox");
        // The store path is a directory: the temp write succeeds but the
        // final rename fails, exercising the failure branch.
        let path = sandbox.join("approvals.json");
        std::fs::create_dir_all(&path).expect("target dir");
        approve_project_server(&path, "/repo", "srv", &server(json!({ "command": "x" })));
        assert!(path.is_dir(), "the target must survive a failed rename");
        let temp_files: Vec<String> = std::fs::read_dir(&sandbox)
            .expect("read sandbox")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(temp_files.is_empty(), "stale temp files: {temp_files:?}");
        let _ = std::fs::remove_dir_all(&sandbox);
    }

    /// The rename step only runs after a fully written temp file.
    #[test]
    fn write_temp_file_fails_when_the_parent_is_missing() {
        let path = std::env::temp_dir()
            .join("rpi-mcp-missing-parent-dir")
            .join("x.tmp");
        assert!(write_temp_file(&path, "{}").is_err());
    }

    /// Round-2: `approvedAt` is a real ISO-8601 UTC timestamp, not epoch
    /// seconds under an ISO-8601 name.
    #[test]
    fn approval_timestamps_are_iso8601() {
        let stamp = now_iso8601();
        assert!(stamp.ends_with('Z'), "{stamp}");
        assert_eq!(stamp.len(), 24, "millisecond precision: {stamp}");
        chrono::DateTime::parse_from_rfc3339(&stamp).expect("RFC 3339 timestamp");
    }

    #[test]
    fn definition_hash_is_key_order_insensitive_and_value_sensitive() {
        let a = server(json!({ "command": "node", "args": ["a"], "env": { "A": "1", "B": "2" } }));
        let b = server(json!({ "env": { "B": "2", "A": "1" }, "args": ["a"], "command": "node" }));
        assert_eq!(
            hash_project_server_definition(&a),
            hash_project_server_definition(&b)
        );
        let c = server(json!({ "command": "node", "args": ["a"] }));
        assert_ne!(
            hash_project_server_definition(&a),
            hash_project_server_definition(&c)
        );
    }

    #[test]
    fn block_reason_texts_match_the_upstream_strings() {
        assert_eq!(
            describe_project_server_block(ProjectServerBlockReason::Untrusted),
            "blocked by project trust — trust the project to review and approve this server"
        );
        assert_eq!(
            describe_project_server_block(ProjectServerBlockReason::ApprovalRequired),
            "blocked: project server approval required — approve it in a trusted interactive session or set user-global settings.projectServers to \"allow\""
        );
        assert_eq!(
            describe_project_server_block(ProjectServerBlockReason::Denied),
            "blocked: project server approval denied — reload in a trusted interactive session to approve it"
        );
    }

    #[test]
    fn untrusted_projects_block_every_project_server_without_confirmation() {
        let loaded = LoadedMcpConfig {
            config: McpConfig {
                mcp_servers: [("proj".to_string(), server(json!({ "command": "x" })))]
                    .into_iter()
                    .collect(),
                imports: None,
                settings: None,
            },
            project_servers: [(
                "proj".to_string(),
                crate::config::ProjectServerSource {
                    path: "/w/.mcp.json".to_string(),
                },
            )]
            .into_iter()
            .collect(),
            project_server_policy: ProjectServerPolicy::Ask,
        };
        let mut confirm_called = false;
        let result = apply_project_server_trust(&loaded, "/w", false, true, |_, _| {
            confirm_called = true;
            true
        });
        assert!(!confirm_called, "untrusted projects never prompt");
        assert_eq!(
            result.blocked["proj"].reason,
            ProjectServerBlockReason::Untrusted
        );
        assert!(result.config.mcp_servers["proj"].is_disabled());
    }

    #[test]
    fn load_time_exclusion_drops_project_servers_only() {
        let loaded = LoadedMcpConfig {
            config: McpConfig {
                mcp_servers: [
                    ("proj".to_string(), server(json!({ "command": "x" }))),
                    ("global".to_string(), server(json!({ "command": "y" }))),
                ]
                .into_iter()
                .collect(),
                imports: None,
                settings: None,
            },
            project_servers: [(
                "proj".to_string(),
                crate::config::ProjectServerSource {
                    path: "/w/.mcp.json".to_string(),
                },
            )]
            .into_iter()
            .collect(),
            project_server_policy: ProjectServerPolicy::Ask,
        };
        let config = exclude_project_servers_at_load_time(&loaded);
        assert!(!config.mcp_servers.contains_key("proj"));
        assert!(config.mcp_servers.contains_key("global"));
    }

    #[test]
    fn non_interactive_trusted_sessions_block_unless_policy_allows() {
        let loaded = LoadedMcpConfig {
            config: McpConfig {
                mcp_servers: [("proj".to_string(), server(json!({ "command": "x" })))]
                    .into_iter()
                    .collect(),
                imports: None,
                settings: None,
            },
            project_servers: [(
                "proj".to_string(),
                crate::config::ProjectServerSource {
                    path: "/w/.mcp.json".to_string(),
                },
            )]
            .into_iter()
            .collect(),
            project_server_policy: ProjectServerPolicy::Ask,
        };
        let result = apply_project_server_trust(&loaded, "/w", true, false, |_, _| true);
        assert_eq!(
            result.blocked["proj"].reason,
            ProjectServerBlockReason::ApprovalRequired
        );

        let allowed = LoadedMcpConfig {
            config: loaded.config.clone(),
            project_servers: loaded.project_servers.clone(),
            project_server_policy: ProjectServerPolicy::Allow,
        };
        let result = apply_project_server_trust(&allowed, "/w", true, false, |_, _| true);
        assert!(result.blocked.is_empty());
        assert!(!result.config.mcp_servers["proj"].is_disabled());
    }

    /// #709 (1f540b9): approvals are shared per repository across git
    /// worktrees; a planted `.git` file without a matching admin back-link
    /// must NOT borrow another checkout's approvals.
    #[test]
    fn approval_scope_shares_worktrees_and_rejects_forged_git_links() {
        let sandbox = std::env::temp_dir().join(format!(
            "rpi-mcp-trust-scope-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let repo = sandbox.join("repo");
        let worktree = sandbox.join("wt");
        let sub = repo.join("packages").join("app");
        std::fs::create_dir_all(repo.join(".git").join("worktrees").join("wt")).expect("repo");
        std::fs::create_dir_all(&worktree).expect("worktree");
        std::fs::create_dir_all(&sub).expect("subdir");
        std::fs::write(repo.join(".git").join("config"), "bare = false\n").expect("config");
        // A linked worktree: `.git` is a regular file pointing at the admin
        // dir, and the admin dir has a `gitdir` file linking back.
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", repo.join(".git/worktrees/wt").display()),
        )
        .expect("worktree git file");
        std::fs::write(
            repo.join(".git/worktrees/wt/gitdir"),
            format!("{}\n", worktree.join(".git").display()),
        )
        .expect("back link");

        let repo_scope = project_approval_scope(&repo.to_string_lossy());
        let worktree_scope = project_approval_scope(&worktree.to_string_lossy());
        let repo_sub_scope = project_approval_scope(&sub.to_string_lossy());
        let canonical_repo = canonical_project_root(&repo.to_string_lossy());
        assert_eq!(repo_scope, canonical_repo);
        assert_eq!(
            worktree_scope, canonical_repo,
            "a linked worktree shares the repository's approval scope"
        );
        assert_eq!(
            repo_sub_scope,
            std::path::Path::new(&canonical_repo)
                .join("packages/app")
                .to_string_lossy(),
            "a subdirectory keeps the repository-relative key"
        );

        // Forged back-link: the admin entry points at a different checkout.
        std::fs::write(
            repo.join(".git/worktrees/wt/gitdir"),
            format!("{}\n", repo.join(".git").display()),
        )
        .expect("forged back link");
        assert_eq!(
            project_approval_scope(&worktree.to_string_lossy()),
            canonical_project_root(&worktree.to_string_lossy()),
            "a forged .git file falls back to its own root"
        );

        // A symlinked `.git` file must not be followed into another
        // checkout (lstat check; upstream lstatSync).
        #[cfg(unix)]
        {
            let alias = sandbox.join("alias");
            std::fs::create_dir_all(&alias).expect("alias dir");
            std::fs::write(repo.join(".git/config"), "bare = false\n").expect("config");
            std::os::unix::fs::symlink(worktree.join(".git"), alias.join(".git")).expect("symlink");
            assert_eq!(
                project_approval_scope(&alias.to_string_lossy()),
                canonical_project_root(&alias.to_string_lossy()),
                "a symlinked .git file does not borrow the target's scope"
            );
        }

        // A linked admin dir whose common dir is bare uses a `git-dir:` key
        // that no canonical path equals (upstream shared-scope guard).
        std::fs::write(
            repo.join(".git/worktrees/wt/gitdir"),
            format!("{}\n", worktree.join(".git").display()),
        )
        .expect("restore back link");
        std::fs::write(repo.join(".git/config"), "bare = true\n").expect("bare config");
        let bare_scope = project_approval_scope(&worktree.to_string_lossy());
        assert!(
            bare_scope.starts_with("git-dir:"),
            "a linked bare admin dir keys the scope by git-dir: {bare_scope}"
        );

        let _ = std::fs::remove_dir_all(&sandbox);
    }

    #[test]
    fn disabled_reason_falls_back_to_the_generic_text() {
        let blocked = std::collections::HashMap::from([(
            "proj".to_string(),
            ProjectServerBlock {
                reason: ProjectServerBlockReason::Denied,
                source: "/w/.mcp.json".to_string(),
            },
        )]);
        assert!(disabled_server_reason(Some(&blocked), "proj").starts_with("blocked: project"));
        assert_eq!(
            disabled_server_reason(Some(&blocked), "other"),
            "disabled. Run /mcp enable other and /reload to enable it."
        );
    }
}
