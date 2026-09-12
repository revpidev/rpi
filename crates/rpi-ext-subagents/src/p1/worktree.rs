//! Managed git worktree isolation for writing children (FR-P1-06).
//!
//! Port of pi-subagents `src/runs/shared/worktree.ts` @ v0.48.0 (56f97234)
//! with the v0.66.0 (0fc0eebb) selective backports of TE16 (R7.1.5):
//! `baseRef` support + fail-closed ref validation (#1842/#1934/#1937,
//! `resolveRepoState`/`normalizeWorktreeBaseRef` worktree.ts:342-366/453-456),
//! binary-complete patches validated before cleanup (#1868, `--binary` +
//! `PATCH_VALIDATION_OPTIONS` worktree.ts:22-23/295-299), and preserved
//! uncertain allocations on setup failure (#1902, `writeWorktreeSetupHandoff`
//! parallel-handoff.ts:619-674). Branch from clean `baseRef` (`git worktree add
//! <base>/rpi-worktree-<runId>-<n> -b rpi-parallel-<runId>-<n> <baseCommit>`),
//! agent cwd = worktree + repo-relative prefix, node_modules symlink + setup
//! hook synthetic paths, patch capture (`git add -A` + `diff --cached --binary
//! <baseCommit>`), handoff manifest (`handoffs/<runId>.json`,
//! parallel-handoff.ts shape), and cleanup that refuses to discard work not
//! represented by a validated handoff patch. git runs through direct
//! `std::process::Command` spawns — same orchestration style as the rpi child
//! processes (design §1.1; the host `exec` envelope is synchronous and cannot
//! serve the async runner — deviation registered in TE05).
//!
//! `exec` capability is therefore NOT requested; nothing here touches the
//! host ABI.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

use crate::config::ExtensionConfig;

/// `runGit` (worktree.ts): cwd-bound git invocation.
fn run_git(cwd: &Path, args: &[&str]) -> std::io::Result<std::process::Output> {
    Command::new("git").args(args).current_dir(cwd).output()
}

fn run_git_checked(cwd: &Path, args: &[&str], context: &str) -> Result<String, String> {
    let output = run_git(cwd, args).map_err(|e| format!("{context}: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        return Err(if stderr.is_empty() { stdout } else { stderr });
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn safe_patch_agent_name(agent: &str) -> String {
    agent
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn build_worktree_branch(run_id: &str, index: usize) -> String {
    format!("rpi-parallel-{run_id}-{index}")
}

/// `normalizeWorktreeBaseRef` rejection text (worktree.ts:455).
pub const BASE_REF_ERROR: &str = "baseRef must be a valid Git ref: use HEAD or a supported named ref (for example, refs/heads/main). Full 40/64-character commit IDs and revision expressions are unsupported.";

/// Handoff diagnostics are bounded and never embed captured output
/// (upstream `writeWorktreeSetupHandoff` comment, parallel-handoff.ts:628-630:
/// "Never copy argv, environment, Error objects or captured stdout/stderr
/// into artifacts"; `diagnostic = text.slice(0, 512)`).
const HANDOFF_DIAGNOSTIC_MAX_CHARS: usize = 512;

fn bounded_handoff_diagnostic(text: &str) -> String {
    if text.chars().count() <= HANDOFF_DIAGNOSTIC_MAX_CHARS {
        return text.to_string();
    }
    let mut bounded: String = text.chars().take(HANDOFF_DIAGNOSTIC_MAX_CHARS).collect();
    bounded.push('…');
    bounded
}

/// `validGitRef` (worktree.ts:445-452) plus the rpi fail-closed leading-dash
/// rule (TE16 §8-3): reject empty/`@`/oversized refs, path escapes, Git
/// revision syntax (`..`/`@{`/`~`/`^`/`:`/`?`/`*`/`[`/`]`/`\\`), control
/// characters and whitespace, and full 40/64-hex commit ids.
fn valid_git_ref(reference: &str) -> bool {
    if reference.is_empty() || reference == "@" || reference.len() > 1024 {
        return false;
    }
    if reference.starts_with('/')
        || reference.ends_with('/')
        || reference.contains("//")
        || reference.contains("..")
        || reference.contains("@{")
    {
        return false;
    }
    // `--end-of-options` covers git's own option parsing; rejecting the dash
    // prefix keeps the contract fail-closed at the plugin boundary too.
    if reference.starts_with('-') {
        return false;
    }
    let full_hash = (reference.len() == 40 || reference.len() == 64)
        && reference.chars().all(|c| c.is_ascii_hexdigit());
    if full_hash {
        return false;
    }
    if reference.chars().any(|c| {
        c.is_control() || c == ' ' || matches!(c, '[' | ']' | '\\' | '~' | '^' | ':' | '?' | '*')
    }) {
        return false;
    }
    if reference.ends_with('.') || reference.ends_with(".lock") {
        return false;
    }
    reference.split('/').all(|component| {
        !component.is_empty()
            && component != "."
            && component != ".."
            && !component.starts_with('.')
            && !component.ends_with('.')
            && !component.ends_with(".lock")
    })
}

/// Normalize and validate a configured worktree base ref without resolving
/// it (worktree.ts:453-456).
pub fn validate_base_ref(reference: &str) -> Result<String, String> {
    if !valid_git_ref(reference) {
        return Err(BASE_REF_ERROR.to_string());
    }
    Ok(reference.to_string())
}

/// `resolveWorktreeBaseDir` (worktree.ts:191-215): config > env
/// `RPI_SUBAGENTS_WORKTREE_DIR` > system temp; relative resolves against the
/// repo root; must not sit inside the extensions dir; created on demand.
pub fn resolve_worktree_base_dir(
    config: &ExtensionConfig,
    repo_root: &Path,
) -> Result<PathBuf, String> {
    let raw = config
        .worktree_base_dir
        .clone()
        .or_else(|| std::env::var("RPI_SUBAGENTS_WORKTREE_DIR").ok())
        .unwrap_or_else(|| crate::paths::temp_dir().to_string_lossy().to_string());
    let trimmed = raw.trim().to_string();
    if trimmed.is_empty() {
        return Err("worktree base directory cannot be empty".to_string());
    }
    let expanded = crate::paths::expand_tilde_and_resolve(&trimmed);
    let resolved = if expanded.is_absolute() {
        expanded
    } else {
        repo_root.join(expanded)
    };
    let extensions_dir = crate::paths::get_agent_dir().join("extensions");
    if resolved.starts_with(&extensions_dir) {
        return Err(format!(
            "worktree base directory cannot be inside the extensions directory: {}. Choose a directory outside it.",
            extensions_dir.to_string_lossy()
        ));
    }
    std::fs::create_dir_all(&resolved).map_err(|e| {
        format!(
            "failed to create worktree base directory {}: {e}",
            resolved.to_string_lossy()
        )
    })?;
    Ok(resolved)
}

fn build_worktree_path(base_dir: &Path, run_id: &str, index: usize) -> PathBuf {
    base_dir.join(format!("rpi-worktree-{run_id}-{index}"))
}

/// `resolveRepoCwdRelative` (worktree.ts:224-234).
pub fn resolve_repo_cwd_relative(cwd: &Path) -> Result<String, String> {
    let check = run_git(cwd, &["rev-parse", "--is-inside-work-tree"]).map_err(|e| e.to_string())?;
    if !check.status.success() || String::from_utf8_lossy(&check.stdout).trim() != "true" {
        return Err("worktree isolation requires a git repository".to_string());
    }
    let raw_prefix = run_git_checked(
        cwd,
        &["rev-parse", "--show-prefix"],
        "rev-parse --show-prefix",
    )?;
    let trimmed = raw_prefix.trim().trim_end_matches(['/', '\\']).to_string();
    if trimmed == "." || trimmed.is_empty() {
        Ok(String::new())
    } else {
        Ok(trimmed)
    }
}

/// One prepared worktree (`WorktreeInfo`).
#[derive(Debug, Clone)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    pub agent_cwd: PathBuf,
    pub branch: String,
    pub index: usize,
    #[allow(dead_code)]
    pub node_modules_linked: bool,
    pub synthetic_paths: Vec<String>,
}

/// `createSingleWorktree` (worktree.ts:380-435): create + node_modules link +
/// setup hook. v0.66 #1902 (R7.1.5.2): a setup failure no longer rolls the
/// allocation back — the worktree and branch are preserved for manual
/// recovery and a `preserved: true` handoff is published.
#[allow(clippy::too_many_arguments)]
pub fn create_worktree(
    toplevel: &Path,
    cwd_relative: &str,
    run_id: &str,
    index: usize,
    base_commit: &str,
    base_dir: &Path,
    agent: Option<&str>,
    config: &ExtensionConfig,
) -> Result<WorktreeInfo, String> {
    let branch = build_worktree_branch(run_id, index);
    let worktree_path = build_worktree_path(base_dir, run_id, index);
    run_git_checked(
        toplevel,
        &[
            "worktree",
            "add",
            &worktree_path.to_string_lossy(),
            "-b",
            &branch,
            base_commit,
        ],
        "git worktree add",
    )
    .map_err(|message| {
        if message.is_empty() {
            format!(
                "failed to create worktree {}",
                worktree_path.to_string_lossy()
            )
        } else {
            message
        }
    })?;

    let agent_cwd = if cwd_relative.is_empty() {
        worktree_path.clone()
    } else {
        worktree_path.join(cwd_relative)
    };
    let mut synthetic_paths = Vec::new();
    let result = (|| -> Result<WorktreeInfo, String> {
        let node_modules_linked = link_node_modules_if_present(toplevel, &worktree_path);
        if node_modules_linked {
            synthetic_paths.push("node_modules".to_string());
        }
        if let Some((hook, timeout_ms)) = config.worktree_setup_hook() {
            let hook = resolve_worktree_setup_hook(&hook, toplevel)?;
            let hook_synthetic = run_worktree_setup_hook(
                &hook,
                timeout_ms,
                toplevel,
                &worktree_path,
                &agent_cwd,
                &branch,
                index,
                run_id,
                base_commit,
                agent,
            )?;
            synthetic_paths.extend(hook_synthetic);
        }
        Ok(WorktreeInfo {
            path: worktree_path.clone(),
            agent_cwd,
            branch: branch.clone(),
            index,
            node_modules_linked,
            synthetic_paths,
        })
    })();

    match result {
        Ok(info) => Ok(info),
        Err(error) => {
            // R7.1.5.2 (#1902): preserve the uncertain allocation. The branch
            // and worktree stay on disk for manual recovery. The error (and
            // therefore the persisted `status.json` step error) carries no
            // captured hook output — upstream never echoes hook stderr
            // (worktree.ts:798-820 reports parse failures only), and G4
            // forbids credentials in error messages/artifacts.
            let handoff = write_preserved_worktree_handoff(
                base_dir,
                run_id,
                index,
                &worktree_path,
                &branch,
                &error,
            );
            let handoff_note = handoff
                .as_ref()
                .map(|path| format!("; handoff: {}", path.to_string_lossy()))
                .unwrap_or_default();
            Err(format!(
                "worktree setup failed for {} (branch {branch}): {error}. Preserved for manual recovery{handoff_note}",
                worktree_path.to_string_lossy()
            ))
        }
    }
}

/// `linkNodeModulesIfPresent` (worktree.ts:243): symlink repo node_modules
/// into the worktree (dependency reuse; excluded from diffs).
fn link_node_modules_if_present(toplevel: &Path, worktree_path: &Path) -> bool {
    let source = toplevel.join("node_modules");
    if !source.exists() {
        return false;
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&source, worktree_path.join("node_modules")).is_ok()
    }
    #[cfg(not(unix))]
    {
        // No symlink fallback on non-unix (dependency reuse is unix-only).
        let _ = worktree_path;
        false
    }
}

/// `validateHookPath` (worktree.ts:268-294): non-empty, `~/` expandable,
/// absolute or repo-relative (a bare name is rejected), must exist and be a
/// file.
fn resolve_worktree_setup_hook(hook: &str, repo_root: &Path) -> Result<PathBuf, String> {
    if hook.trim().is_empty() {
        return Err("worktree setup hook path cannot be empty".to_string());
    }
    let expanded = if let Some(rest) = hook.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_default();
        PathBuf::from(home).join(rest)
    } else {
        PathBuf::from(hook)
    };
    let resolved = if expanded.is_absolute() {
        expanded
    } else if hook.contains('/') || hook.contains('\\') {
        repo_root.join(expanded)
    } else {
        return Err(
            "worktree setup hook must be an absolute path or a repo-relative path".to_string(),
        );
    };
    if !resolved.exists() {
        return Err(format!(
            "worktree setup hook not found: {}",
            resolved.display()
        ));
    }
    if resolved.is_dir() {
        return Err(format!(
            "worktree setup hook must be a file, got directory: {}",
            resolved.display()
        ));
    }
    Ok(resolved)
}

/// `runWorktreeSetupHook` (worktree.ts:336-379): stdin = the v1 payload,
/// stdout = JSON object `{ syntheticPaths?: string[] }`; non-zero exit,
/// malformed stdout, or timeout (default 30s) fail the setup.
#[allow(clippy::too_many_arguments)]
fn run_worktree_setup_hook(
    hook: &Path,
    timeout_ms: u64,
    toplevel: &Path,
    worktree_path: &Path,
    agent_cwd: &Path,
    branch: &str,
    index: usize,
    run_id: &str,
    base_commit: &str,
    agent: Option<&str>,
) -> Result<Vec<String>, String> {
    let stdin = json!({
        "version": 1,
        "repoRoot": toplevel.to_string_lossy(),
        "worktreePath": worktree_path.to_string_lossy(),
        "agentCwd": agent_cwd.to_string_lossy(),
        "branch": branch,
        "index": index,
        "runId": run_id,
        "baseCommit": base_commit,
        "agent": agent,
    })
    .to_string();
    let child = Command::new(hook)
        .current_dir(worktree_path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("worktree setup hook failed to start: {e}"))?;
    // Timeout via a watchdog thread (spawnSync-equivalent): the waiter runs
    // on its own thread, the watchdog polls it. On timeout the hook process
    // is killed (Node spawnSync {timeout} sends its killSignal) — a runaway
    // hook must not outlive the worktree it was mutating.
    let hook_pid = child.id();
    let mut child = child;
    if let Some(mut stdin_handle) = child.stdin.take() {
        use std::io::Write;
        let _ = stdin_handle.write_all(stdin.as_bytes());
    }
    let waiter = std::thread::spawn(move || child.wait_with_output());
    let output = wait_with_timeout(
        waiter,
        std::time::Duration::from_millis(timeout_ms),
        hook_pid,
    )
    .map_err(|_| format!("worktree setup hook timed out after {timeout_ms}ms"))?;
    if !output.status.success() {
        // The hook's captured stderr is deliberately discarded: upstream
        // never echoes it (worktree.ts:798-820), and the error reaches the
        // persisted status document (`step["error"]`), so G4 credential
        // hygiene wins over the extra diagnostic. The exit code plus the
        // worktree path/handoff are enough to reproduce by running the hook
        // manually.
        return Err(format!(
            "worktree setup hook failed (exit {})",
            output
                .status
                .code()
                .map(|code| code.to_string())
                .unwrap_or_else(|| "signal".to_string())
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Value = serde_json::from_str(stdout.trim())
        .map_err(|_| "worktree setup hook stdout must be a JSON object".to_string())?;
    if !parsed.is_object() {
        return Err("worktree setup hook stdout must be a JSON object".to_string());
    }
    Ok(parsed
        .get("syntheticPaths")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default())
}

fn wait_with_timeout(
    waiter: std::thread::JoinHandle<std::io::Result<std::process::Output>>,
    timeout: std::time::Duration,
    hook_pid: u32,
) -> std::io::Result<std::process::Output> {
    // The watchdog polls the join handle; on timeout the hook process is
    // killed through the SIGTERM → 500ms → SIGKILL ladder before returning —
    // leaving it alive would race the worktree rollback deletion.
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if waiter.is_finished() {
            return waiter
                .join()
                .map_err(|_| std::io::Error::other("hook thread panicked"))?;
        }
        if std::time::Instant::now() >= deadline {
            kill_hook_process(hook_pid);
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "hook timeout",
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Signal ladder for a timed-out hook process (SIGTERM, short grace, then
/// SIGKILL — Node's spawnSync timeout sends its default killSignal SIGTERM;
/// the KILL backstop covers hooks that trap TERM).
fn kill_hook_process(pid: u32) {
    #[cfg(unix)]
    {
        if pid == 0 {
            return;
        }
        // Safety: kill(2) with a checked pid.
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        while std::time::Instant::now() < deadline {
            if unsafe { libc::kill(pid as i32, 0) } != 0 {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
    }
}

/// `normalizeSyntheticPath` (worktree.ts:297-315): relative only, no escapes,
/// not repo root; synthetic paths are unlinked before diffs/cleanup.
///
/// P1-3: the validation is load-bearing, not documentation — a
/// `worktreeSetupHook` returning an absolute path or a `..` escape must be
/// rejected (skipped with a warning) instead of unlinking outside the
/// worktree: `Path::join` replaces the base entirely for absolute inputs
/// and `..` components escape it. Mirrors the upstream checks verbatim
/// (empty / absolute / worktree root / `..` escape → error).
fn normalize_synthetic_path(worktree_path: &Path, raw_path: &str) -> Option<PathBuf> {
    let trimmed = raw_path.trim();
    if trimmed.is_empty() {
        tracing::warn!("synthetic path cannot be empty: {raw_path:?} — skipped");
        return None;
    }
    let candidate = Path::new(trimmed);
    if candidate.is_absolute() {
        tracing::warn!("synthetic path must be relative: {raw_path:?} — skipped");
        return None;
    }
    // Reject escapes before touching the filesystem (upstream uses
    // path.resolve + path.relative; the component walk is the Rust
    // equivalent without requiring the path to exist).
    let mut resolved = worktree_path.to_path_buf();
    for component in candidate.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::Normal(part) => resolved.push(part),
            std::path::Component::ParentDir => {
                tracing::warn!("synthetic path escapes the worktree root: {raw_path:?} — skipped");
                return None;
            }
            // Prefix/root cannot appear for a relative input, but stay
            // fail-closed if one ever slips through.
            _ => {
                tracing::warn!("synthetic path must be relative: {raw_path:?} — skipped");
                return None;
            }
        }
    }
    if resolved == worktree_path {
        tracing::warn!("synthetic path cannot target the worktree root: {raw_path:?} — skipped");
        return None;
    }
    Some(resolved)
}

fn remove_synthetic_paths(worktree: &WorktreeInfo) {
    for raw in &worktree.synthetic_paths {
        let Some(path) = normalize_synthetic_path(&worktree.path, raw) else {
            continue;
        };
        if path.is_dir() && !path.is_symlink() {
            let _ = std::fs::remove_dir_all(&path);
        } else {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Captured diff (`WorktreeDiff`).
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct WorktreeDiff {
    pub index: usize,
    pub agent: String,
    pub branch: String,
    pub diff_stat: String,
    pub files_changed: usize,
    pub insertions: usize,
    pub deletions: usize,
    pub patch_path: PathBuf,
}

/// `validateWorktreePatch` (worktree.ts:295-299): `PATCH_VALIDATION_OPTIONS`
/// (worktree.ts:23) check the captured patch against the staged index in
/// reverse — the worktree already contains the changes, so a forward apply
/// would fail by construction. `--binary` keeps binary hunks applyable.
pub fn validate_worktree_patch(worktree_path: &Path, patch_path: &Path) -> Result<(), String> {
    let patch_arg = patch_path.to_string_lossy().to_string();
    let output = run_git(
        worktree_path,
        &[
            "apply",
            "--check",
            "--cached",
            "--reverse",
            "--binary",
            "--whitespace=nowarn",
            &patch_arg,
        ],
    )
    .map_err(|e| format!("git apply --check failed to start: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Err(if stderr.is_empty() { stdout } else { stderr })
}

/// `captureWorktreeDiff` (worktree.ts:509-537): add -A, three diff flavors
/// against the base commit, patch file written next to the manifest. v0.66
/// #1868 (R7.1.5.1): the patch is binary-complete (`--binary`) and validated
/// before it is accepted as the handoff record.
pub fn capture_worktree_diff(
    worktree: &WorktreeInfo,
    agent: &str,
    base_commit: &str,
    patch_dir: &Path,
) -> Result<WorktreeDiff, String> {
    remove_synthetic_paths(worktree);
    run_git_checked(&worktree.path, &["add", "-A"], "git add -A")?;
    let diff_stat = run_git_checked(
        &worktree.path,
        &["diff", "--cached", "--stat", base_commit],
        "git diff --stat",
    )?
    .trim()
    .to_string();
    let patch = run_git_checked(
        &worktree.path,
        &["diff", "--cached", "--binary", base_commit],
        "git diff",
    )?;
    let numstat = run_git_checked(
        &worktree.path,
        &["diff", "--cached", "--numstat", base_commit],
        "git diff --numstat",
    )?;
    let _ = std::fs::create_dir_all(patch_dir);
    let patch_path = patch_dir.join(format!(
        "{}-{}.patch",
        safe_patch_agent_name(agent),
        worktree.index
    ));
    let _ = std::fs::write(&patch_path, &patch);
    if patch.trim().is_empty() {
        return Ok(WorktreeDiff {
            index: worktree.index,
            agent: agent.to_string(),
            branch: worktree.branch.clone(),
            diff_stat,
            files_changed: 0,
            insertions: 0,
            deletions: 0,
            patch_path,
        });
    }
    // A patch that cannot be applied back to the staged work is not a usable
    // handoff record; refusing here makes cleanup preserve the worktree
    // instead of discarding work the patch does not represent.
    if let Err(reason) = validate_worktree_patch(&worktree.path, &patch_path) {
        return Err(format!(
            "captured worktree patch is not machine-applyable: {reason} (patch: {})",
            patch_path.to_string_lossy()
        ));
    }
    let mut files_changed = 0usize;
    let mut insertions = 0usize;
    let mut deletions = 0usize;
    for line in numstat.lines() {
        let mut parts = line.split('\t');
        let (Some(raw_insertions), Some(raw_deletions)) = (parts.next(), parts.next()) else {
            continue;
        };
        files_changed += 1;
        if let Ok(value) = raw_insertions.parse::<usize>() {
            insertions += value;
        }
        if let Ok(value) = raw_deletions.parse::<usize>() {
            deletions += value;
        }
    }
    Ok(WorktreeDiff {
        index: worktree.index,
        agent: agent.to_string(),
        branch: worktree.branch.clone(),
        diff_stat,
        files_changed,
        insertions,
        deletions,
        patch_path,
    })
}

/// One cleanup outcome recorded in the handoff manifest (upstream
/// `WorktreeCleanupTask`, worktree.ts:80-87 + parallel-handoff.ts:497-616).
#[derive(Debug, Clone)]
pub struct WorktreeCleanupTask {
    pub index: usize,
    pub path: PathBuf,
    pub branch: String,
    pub worktree_removed: bool,
    pub branch_removed: bool,
    pub preserved: bool,
    pub reason: Option<String>,
}

impl WorktreeCleanupTask {
    /// First-pass record: cleanup has not run yet (upstream writes
    /// `preserved: true` / `"cleanup pending durable handoff capture"`).
    pub fn pending(index: usize, path: &Path, branch: &str) -> Self {
        Self {
            index,
            path: path.to_path_buf(),
            branch: branch.to_string(),
            worktree_removed: false,
            branch_removed: false,
            preserved: true,
            reason: Some("cleanup pending durable handoff capture".to_string()),
        }
    }

    pub fn removed(index: usize, path: &Path, branch: &str) -> Self {
        Self {
            index,
            path: path.to_path_buf(),
            branch: branch.to_string(),
            worktree_removed: true,
            branch_removed: true,
            preserved: false,
            reason: None,
        }
    }

    pub fn preserved(index: usize, path: &Path, branch: &str, reason: &str) -> Self {
        Self {
            index,
            path: path.to_path_buf(),
            branch: branch.to_string(),
            worktree_removed: false,
            branch_removed: false,
            preserved: true,
            reason: Some(reason.to_string()),
        }
    }

    fn to_json(&self) -> Value {
        let mut value = json!({
            "index": self.index,
            "path": self.path.to_string_lossy(),
            "branch": self.branch,
            "worktreeRemoved": self.worktree_removed,
            "branchRemoved": self.branch_removed,
            "preserved": self.preserved,
        });
        if let Some(reason) = &self.reason {
            // Handoff artifacts never carry unbounded diagnostics.
            value["reason"] = json!(bounded_handoff_diagnostic(reason));
        }
        value
    }
}

/// Setup-failure handoff (upstream `writeWorktreeSetupHandoff`,
/// parallel-handoff.ts:619-674): the uncertain allocation is preserved for
/// manual recovery, never removed (R7.1.5.2 / #1902). Written to a
/// per-allocation file so a later batch manifest for the same run cannot
/// clobber the evidence.
fn write_preserved_worktree_handoff(
    base_dir: &Path,
    run_id: &str,
    index: usize,
    worktree_path: &Path,
    branch: &str,
    reason: &str,
) -> Option<PathBuf> {
    let handoff_dir = base_dir.join("handoffs");
    if std::fs::create_dir_all(&handoff_dir).is_err() {
        return None;
    }
    let manifest = json!({
        "version": 1,
        "runId": run_id,
        "mode": "parallel",
        "source": "foreground",
        "createdAt": crate::artifacts::format_iso8601(crate::artifacts::now_millis()),
        "groups": [{
            "stepIndex": 0,
            "baseCommit": Value::Null,
            "repoRoot": Value::Null,
            "children": [],
            "cleanup": {
                "state": "partial",
                "tasks": [WorktreeCleanupTask::preserved(index, worktree_path, branch, reason).to_json()],
                "pruned": false,
            },
        }],
    });
    let path = handoff_dir.join(format!("{run_id}-preserved-{index}.json"));
    crate::artifacts::write_metadata(&path, &manifest).ok()?;
    Some(path)
}

/// `writeParallelHandoffGroup` (parallel-handoff.ts:497-616) — the manifest
/// survives cleanup so the orchestrator can apply/inspect patches later. The
/// writer is called twice per batch (patch records first, then the cleanup
/// report), matching the upstream two-pass ordering that lets cleanup verify
/// a patch is journaled before it removes anything.
pub fn write_handoff_manifest(
    base_dir: &Path,
    run_id: &str,
    mode: &str,
    cwd: &Path,
    base_commit: &str,
    children: &[(usize, String, String, WorktreeDiff)],
    cleanup_tasks: &[WorktreeCleanupTask],
) -> PathBuf {
    let handoff_dir = base_dir.join("handoffs");
    let _ = std::fs::create_dir_all(&handoff_dir);
    let cleanup_complete = cleanup_tasks
        .iter()
        .all(|task| task.worktree_removed && task.branch_removed && !task.preserved);
    let manifest = json!({
        "version": 1,
        "runId": run_id,
        "mode": mode,
        "source": "foreground",
        "cwd": cwd.to_string_lossy(),
        "createdAt": crate::artifacts::format_iso8601(crate::artifacts::now_millis()),
        "groups": [{
            "stepIndex": 0,
            "baseCommit": base_commit,
            "repoRoot": cwd.to_string_lossy(),
            "children": children.iter().map(|(index, agent, status, diff)| json!({
                "index": index,
                "agent": agent,
                "status": status,
                "patch": {
                    "path": diff.patch_path.to_string_lossy(),
                    "branch": diff.branch,
                    "changed": diff.files_changed > 0,
                    "diffStat": diff.diff_stat,
                    "filesChanged": diff.files_changed,
                    "insertions": diff.insertions,
                    "deletions": diff.deletions,
                },
            })).collect::<Vec<_>>(),
            "cleanup": {
                "state": if cleanup_complete { "complete" } else { "partial" },
                "tasks": cleanup_tasks.iter().map(WorktreeCleanupTask::to_json).collect::<Vec<_>>(),
                "pruned": false,
            },
        }],
    });
    let path = handoff_dir.join(format!("{run_id}.json"));
    // TE17 R7.1.7.1: the handoff manifest is an auxiliary artifact — an
    // exhausted write logs (and the caller keeps the path it was given)
    // instead of dropping the failure silently.
    if let Err(error) = crate::artifacts::write_metadata(&path, &manifest) {
        tracing::warn!(
            path = %path.display(),
            error = %error,
            "worktree handoff manifest write failed after retrying"
        );
    }
    path
}

/// `handoffRecordsPatch` (worktree.ts:1074-1086): the manifest journals this
/// exact patch path as an error-free child record.
fn handoff_records_patch(manifest_path: Option<&Path>, patch_path: &Path) -> bool {
    let Some(manifest_path) = manifest_path else {
        return false;
    };
    let Ok(raw) = std::fs::read_to_string(manifest_path) else {
        return false;
    };
    let Ok(manifest) = serde_json::from_str::<Value>(&raw) else {
        return false;
    };
    if manifest.get("version").and_then(Value::as_u64) != Some(1) {
        return false;
    }
    let resolved = std::fs::canonicalize(patch_path).unwrap_or_else(|_| patch_path.to_path_buf());
    manifest
        .get("groups")
        .and_then(Value::as_array)
        .is_some_and(|groups| {
            groups.iter().any(|group| {
                group
                    .get("children")
                    .and_then(Value::as_array)
                    .is_some_and(|children| {
                        children.iter().any(|child| {
                            let patch = child.get("patch");
                            patch.and_then(|p| p.get("error")).is_none()
                                && patch
                                    .and_then(|p| p.get("path"))
                                    .and_then(Value::as_str)
                                    .is_some_and(|path| {
                                        std::fs::canonicalize(path)
                                            .unwrap_or_else(|_| PathBuf::from(path))
                                            == resolved
                                    })
                        })
                    })
            })
        })
}

/// `cleanupSingleWorktree` (worktree.ts:1122-1265) preserve form: synthetic
/// paths removed first, then any residual work (porcelain status or diff
/// against the base) must be represented by a handoff-journaled patch that
/// still applies in reverse; otherwise the worktree/branch stay on disk.
pub fn cleanup_worktree(
    toplevel: &Path,
    worktree: &WorktreeInfo,
    base_commit: &str,
    patch_path: Option<&Path>,
    manifest_path: Option<&Path>,
) -> Result<(), String> {
    remove_synthetic_paths(worktree);
    let status = run_git(&worktree.path, &["status", "--porcelain"]).map_err(|e| {
        format!(
            "git status failed in {}: {e}",
            worktree.path.to_string_lossy()
        )
    })?;
    if !status.status.success() {
        return Err(format!(
            "worktree {} preserved: git status failed ({})",
            worktree.path.to_string_lossy(),
            String::from_utf8_lossy(&status.stderr).trim()
        ));
    }
    let dirty = !String::from_utf8_lossy(&status.stdout).trim().is_empty();
    let base_diff =
        run_git(&worktree.path, &["diff", "--quiet", base_commit, "--"]).map_err(|e| {
            format!(
                "git diff failed in {}: {e}",
                worktree.path.to_string_lossy()
            )
        })?;
    let diff_code = base_diff.status.code();
    if !matches!(diff_code, Some(0) | Some(1)) {
        return Err(format!(
            "worktree {} preserved: git diff check failed ({})",
            worktree.path.to_string_lossy(),
            String::from_utf8_lossy(&base_diff.stderr).trim()
        ));
    }
    let has_work = dirty || diff_code == Some(1);
    if has_work {
        let patch_ok = patch_path.is_some_and(|path| {
            path.exists()
                && std::fs::metadata(path)
                    .map(|meta| meta.len() > 0)
                    .unwrap_or(false)
                && handoff_records_patch(manifest_path, path)
                && validate_worktree_patch(&worktree.path, path).is_ok()
        });
        if !patch_ok {
            return Err(format!(
                "worktree {} preserved: uncommitted changes are not represented by a validated handoff patch; manual recovery required",
                worktree.path.to_string_lossy()
            ));
        }
    }
    run_git_checked(
        toplevel,
        &[
            "worktree",
            "remove",
            "--force",
            &worktree.path.to_string_lossy(),
        ],
        "git worktree remove",
    )?;
    run_git_checked(
        toplevel,
        &["branch", "-D", &worktree.branch],
        "git branch -D",
    )?;
    Ok(())
}

/// `resolveRepoState` (worktree.ts:342-366): toplevel + base commit resolved
/// from the validated `baseRef` (default HEAD), so `git worktree add` and
/// patch capture share one commit.
pub fn resolve_repo_base_with_ref(
    cwd: &Path,
    base_ref: Option<&str>,
) -> Result<(PathBuf, String), String> {
    let toplevel = run_git_checked(
        cwd,
        &["rev-parse", "--show-toplevel"],
        "git rev-parse --show-toplevel",
    )?
    .trim()
    .to_string();
    let base_commit = match base_ref {
        Some(reference) => {
            let validated = validate_base_ref(reference)?;
            let spec = format!("{validated}^{{commit}}");
            let output = run_git(cwd, &["rev-parse", "--verify", "--end-of-options", &spec])
                .map_err(|e| {
                    format!("baseRef '{validated}' could not be resolved to a commit: {e}")
                })?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                let detail = if stderr.is_empty() {
                    String::from_utf8_lossy(&output.stdout).trim().to_string()
                } else {
                    stderr
                };
                return Err(format!(
                    "baseRef '{validated}' could not be resolved to a commit: {detail}"
                ));
            }
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }
        None => run_git_checked(cwd, &["rev-parse", "HEAD"], "git rev-parse HEAD")?
            .trim()
            .to_string(),
    };
    if base_commit.is_empty() {
        return Err("worktree base commit could not be resolved".to_string());
    }
    Ok((PathBuf::from(toplevel), base_commit))
}

/// Resolve the repo toplevel + base commit for a run at the current HEAD.
#[cfg(test)]
fn resolve_repo_base(cwd: &Path) -> Result<(PathBuf, String), String> {
    resolve_repo_base_with_ref(cwd, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_available() -> bool {
        Command::new("git").arg("--version").output().is_ok()
    }

    #[test]
    fn branch_and_path_naming() {
        assert_eq!(build_worktree_branch("ab12", 2), "rpi-parallel-ab12-2");
        assert_eq!(
            build_worktree_path(Path::new("/tmp/wt"), "ab12", 2),
            PathBuf::from("/tmp/wt/rpi-worktree-ab12-2")
        );
        assert_eq!(safe_patch_agent_name("scout/reviewer"), "scout_reviewer");
    }

    #[test]
    fn resolve_worktree_setup_hook_validates() {
        let dir = std::env::temp_dir().join(format!("rpi-sub-hook-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Bare name (no separator) is rejected (worktree.ts:280-282).
        assert!(resolve_worktree_setup_hook("bare-hook", &dir).is_err());
        // Missing path is rejected.
        assert!(resolve_worktree_setup_hook("./missing-hook", &dir).is_err());
        // Directory is rejected.
        assert!(resolve_worktree_setup_hook("./sub", &dir).is_err());
        // Existing executable file resolves (absolute and repo-relative).
        let hook = dir.join("hook.sh");
        std::fs::write(&hook, "#!/bin/sh\nsleep 0\n").unwrap();
        let resolved = resolve_worktree_setup_hook("./hook.sh", &dir).unwrap();
        assert_eq!(resolved, hook);
        let absolute = resolve_worktree_setup_hook(&hook.to_string_lossy(), &dir).unwrap();
        assert_eq!(absolute, hook);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn timed_out_hook_process_is_killed() {
        // A hook that ignores SIGTERM must still die within the ladder:
        // trap '' TERM keeps it alive past the grace, SIGKILL ends it.
        let dir = std::env::temp_dir().join(format!("rpi-sub-hookkill-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let hook = dir.join("stubborn-hook.sh");
        std::fs::write(&hook, "#!/bin/sh\ntrap '' TERM\nsleep 30\necho '{}'\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        let started = std::time::Instant::now();
        let result = run_worktree_setup_hook(
            &hook, 300, // ms — well under the hook's 30s sleep
            &dir, &dir, &dir, "branch", 0, "run", "deadbeef", None,
        );
        assert!(result.is_err());
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        // The hook process is gone: no `sh -c "sleep 30"` from this test is
        // left behind. The waiter thread joined, so wait_with_output reaped it.
        let leftover = Command::new("sh")
            .arg("-c")
            .arg(format!(
                "pgrep -f '{}' | grep -v $$ || true",
                hook.display()
            ))
            .output()
            .unwrap();
        assert!(
            leftover.stdout.is_empty(),
            "timed-out hook should have been killed, found: {}",
            String::from_utf8_lossy(&leftover.stdout)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn test_repo(tag: &str) -> (PathBuf, PathBuf, String) {
        let dir = std::env::temp_dir().join(format!(
            "rpi-sub-wt-{tag}-{}-{}",
            std::process::id(),
            crate::artifacts::now_millis()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        run_git_checked(&repo, &["init", "-q"], "init").unwrap();
        run_git_checked(&repo, &["config", "user.email", "t@t"], "config").unwrap();
        run_git_checked(&repo, &["config", "user.name", "t"], "config").unwrap();
        std::fs::write(repo.join("base.txt"), "base").unwrap();
        run_git_checked(&repo, &["add", "-A"], "add").unwrap();
        run_git_checked(&repo, &["commit", "-q", "-m", "base"], "commit").unwrap();
        let (toplevel, base_commit) = resolve_repo_base(&repo).unwrap();
        (dir, toplevel, base_commit)
    }

    fn branch_exists(repo: &Path, branch: &str) -> bool {
        run_git(
            repo,
            &[
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ],
        )
        .map(|output| output.status.success())
        .unwrap_or(false)
    }

    /// Config pinned to a per-test worktree base dir: the default temp base
    /// is process-global and preserved allocations (R7.1.5.2) outlive the
    /// test, so tests must not share it.
    fn test_config(dir: &Path) -> ExtensionConfig {
        ExtensionConfig {
            worktree_base_dir: Some(dir.join("worktrees").to_string_lossy().to_string()),
            ..ExtensionConfig::default()
        }
    }

    #[test]
    fn base_ref_validation_rules() {
        // T-6: fail-closed shape validation before any git call.
        for valid in ["HEAD", "main", "refs/heads/main", "v1.0.0", "feature/x-1"] {
            assert_eq!(validate_base_ref(valid).unwrap(), valid, "{valid}");
        }
        for invalid in [
            "",
            " ",
            "-b",
            "--help",
            "a b",
            "HEAD~1",
            "HEAD^2",
            "a..b",
            "@{upstream}",
            "refs/heads/",
            "/refs/heads/main",
            "a//b",
            ".hidden",
            "a.",
            "a.lock",
            "a:b",
            "a*b",
            "a?b",
            "a[b]",
            "a\\b",
            "@",
            "0123456789012345678901234567890123456789",
            &"0".repeat(64),
        ] {
            assert!(validate_base_ref(invalid).is_err(), "{invalid:?}");
        }
        // Unknown-but-well-formed refs pass shape validation and fail at
        // resolution (fail-closed with the ref in the diagnostic).
        let (dir, toplevel, _) = test_repo("badref");
        let error = resolve_repo_base_with_ref(&toplevel, Some("refs/heads/no-such-ref"))
            .expect_err("unknown ref must fail resolution");
        assert!(error.contains("refs/heads/no-such-ref"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn binary_patch_is_complete_and_applyable() {
        if !git_available() {
            return;
        }
        // T-1: a real binary file (non-UTF8 bytes, not all-zero) must land in
        // the patch as a GIT binary patch and restore from the base checkout.
        let (dir, toplevel, base_commit) = test_repo("binary");
        let config = test_config(&dir);
        let base_dir = resolve_worktree_base_dir(&config, &toplevel).unwrap();
        let worktree = create_worktree(
            &toplevel,
            "",
            "runbin",
            0,
            &base_commit,
            &base_dir,
            Some("worker"),
            &config,
        )
        .unwrap();
        let binary: Vec<u8> = (0u8..=255).cycle().take(4096).collect();
        std::fs::write(worktree.path.join("blob.bin"), &binary).unwrap();
        let patch_dir = dir.join("patches");
        let diff = capture_worktree_diff(&worktree, "worker", &base_commit, &patch_dir).unwrap();
        let patch_text = std::fs::read_to_string(&diff.patch_path).unwrap();
        assert!(
            patch_text.contains("GIT binary patch"),
            "binary payload must be captured: {patch_text}"
        );
        assert!(validate_worktree_patch(&worktree.path, &diff.patch_path).is_ok());
        let manifest = write_handoff_manifest(
            &base_dir,
            "runbin",
            "parallel",
            &toplevel,
            &base_commit,
            &[(
                0,
                "worker".to_string(),
                "complete".to_string(),
                diff.clone(),
            )],
            &[WorktreeCleanupTask::pending(
                0,
                &worktree.path,
                &worktree.branch,
            )],
        );
        cleanup_worktree(
            &toplevel,
            &worktree,
            &base_commit,
            Some(&diff.patch_path),
            Some(&manifest),
        )
        .unwrap();
        assert!(!worktree.path.exists());
        // The main checkout is at the base state, so the captured patch must
        // apply cleanly there — binary changes included.
        let patch_arg = diff.patch_path.to_string_lossy().to_string();
        let check = run_git(&toplevel, &["apply", "--check", &patch_arg]).unwrap();
        assert!(
            check.status.success(),
            "patch must restore the binary change: {}",
            String::from_utf8_lossy(&check.stderr)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cleanup_preserves_unrepresented_work() {
        if !git_available() {
            return;
        }
        // T-2/T-3: a dirty worktree whose patch is missing, empty, or
        // corrupted is preserved (worktree + branch stay) instead of being
        // silently deleted.
        let (dir, toplevel, base_commit) = test_repo("preserve");
        let config = test_config(&dir);
        let base_dir = resolve_worktree_base_dir(&config, &toplevel).unwrap();
        let worktree = create_worktree(
            &toplevel,
            "",
            "runkeep",
            0,
            &base_commit,
            &base_dir,
            Some("worker"),
            &config,
        )
        .unwrap();
        std::fs::write(worktree.path.join("feature.txt"), "change").unwrap();
        let patch_dir = dir.join("patches");
        let diff = capture_worktree_diff(&worktree, "worker", &base_commit, &patch_dir).unwrap();
        let manifest = write_handoff_manifest(
            &base_dir,
            "runkeep",
            "parallel",
            &toplevel,
            &base_commit,
            &[(
                0,
                "worker".to_string(),
                "complete".to_string(),
                diff.clone(),
            )],
            &[WorktreeCleanupTask::pending(
                0,
                &worktree.path,
                &worktree.branch,
            )],
        );
        // No manifest: the patch is not journaled, so cleanup refuses.
        let error = cleanup_worktree(
            &toplevel,
            &worktree,
            &base_commit,
            Some(&diff.patch_path),
            None,
        )
        .expect_err("unrecorded work must be preserved");
        assert!(
            error.contains(&worktree.path.to_string_lossy().to_string()),
            "{error}"
        );
        // Corrupted patch: recorded but no longer applyable.
        std::fs::write(&diff.patch_path, "not a patch").unwrap();
        let error = cleanup_worktree(
            &toplevel,
            &worktree,
            &base_commit,
            Some(&diff.patch_path),
            Some(&manifest),
        )
        .expect_err("invalid patch must be preserved");
        assert!(error.contains("preserved"), "{error}");
        // Empty patch with residual work: still preserved.
        std::fs::write(&diff.patch_path, "").unwrap();
        let error = cleanup_worktree(
            &toplevel,
            &worktree,
            &base_commit,
            Some(&diff.patch_path),
            Some(&manifest),
        )
        .expect_err("empty patch must be preserved");
        assert!(error.contains("preserved"), "{error}");
        assert!(worktree.path.exists(), "worktree kept for inspection");
        assert!(branch_exists(&toplevel, &worktree.branch), "branch kept");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn setup_failure_preserves_allocation() {
        if !git_available() {
            return;
        }
        // T-4: a failing setup hook must not roll the worktree/branch back.
        let (dir, toplevel, base_commit) = test_repo("setupfail");
        let hook = toplevel.join("failing-hook.sh");
        // The stderr carries a secret-shaped token: it must reach neither
        // the returned error (persisted as `step["error"]`) nor the handoff
        // artifact (G4 red line; upstream never echoes hook stderr).
        std::fs::write(
            &hook,
            "#!/bin/sh\necho 'token=super-secret-hook-token' >&2\nexit 3\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut config = test_config(&dir);
        config.worktree_setup_hook = Some(json!("./failing-hook.sh"));
        let base_dir = resolve_worktree_base_dir(&config, &toplevel).unwrap();
        let error = create_worktree(
            &toplevel,
            "",
            "runsetup",
            0,
            &base_commit,
            &base_dir,
            Some("worker"),
            &config,
        )
        .expect_err("hook failure must surface");
        let worktree_path = build_worktree_path(&base_dir, "runsetup", 0);
        let branch = build_worktree_branch("runsetup", 0);
        assert!(
            error.contains(&worktree_path.to_string_lossy().to_string()),
            "{error}"
        );
        assert!(error.contains("Preserved for manual recovery"), "{error}");
        assert!(
            !error.contains("super-secret-hook-token"),
            "caller error must not echo captured hook stderr: {error}"
        );
        assert!(error.contains("hook failed (exit 3)"), "{error}");
        assert!(worktree_path.exists(), "allocation preserved");
        assert!(branch_exists(&toplevel, &branch), "branch preserved");
        let handoff = base_dir.join("handoffs").join("runsetup-preserved-0.json");
        let raw = std::fs::read_to_string(&handoff).expect("preserved handoff written");
        let manifest: Value = serde_json::from_str(&raw).unwrap();
        let task = &manifest["groups"][0]["cleanup"]["tasks"][0];
        assert_eq!(task["preserved"], json!(true), "{manifest}");
        assert_eq!(task["worktreeRemoved"], json!(false));
        assert_eq!(task["branchRemoved"], json!(false));
        let reason = task["reason"].as_str().unwrap();
        assert!(reason.contains("hook"), "{manifest}");
        assert!(
            !reason.contains("super-secret-hook-token"),
            "handoff must not persist captured stderr: {manifest}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clean_worktree_without_changes_is_reclaimed() {
        if !git_available() {
            return;
        }
        // T-2 (upstream `hasWork`): an empty patch with no work is reclaimed,
        // not preserved — otherwise every no-change child would leak an
        // allocation.
        let (dir, toplevel, base_commit) = test_repo("clean");
        let config = test_config(&dir);
        let base_dir = resolve_worktree_base_dir(&config, &toplevel).unwrap();
        let worktree = create_worktree(
            &toplevel,
            "",
            "runclean",
            0,
            &base_commit,
            &base_dir,
            Some("worker"),
            &config,
        )
        .unwrap();
        let patch_dir = dir.join("patches");
        let diff = capture_worktree_diff(&worktree, "worker", &base_commit, &patch_dir).unwrap();
        assert_eq!(diff.files_changed, 0);
        let manifest = write_handoff_manifest(
            &base_dir,
            "runclean",
            "parallel",
            &toplevel,
            &base_commit,
            &[(
                0,
                "worker".to_string(),
                "complete".to_string(),
                diff.clone(),
            )],
            &[WorktreeCleanupTask::pending(
                0,
                &worktree.path,
                &worktree.branch,
            )],
        );
        cleanup_worktree(
            &toplevel,
            &worktree,
            &base_commit,
            Some(&diff.patch_path),
            Some(&manifest),
        )
        .expect("clean worktree is reclaimed");
        assert!(!worktree.path.exists());
        assert!(!branch_exists(&toplevel, &worktree.branch));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn base_ref_selects_commit() {
        if !git_available() {
            return;
        }
        // T-5: a named baseRef (not HEAD) is resolved and checked out.
        let (dir, toplevel, first_commit) = test_repo("baseref");
        run_git_checked(
            &toplevel,
            &["branch", "base-at-first", &first_commit],
            "git branch",
        )
        .unwrap();
        std::fs::write(toplevel.join("second.txt"), "second").unwrap();
        run_git_checked(&toplevel, &["add", "-A"], "add").unwrap();
        run_git_checked(&toplevel, &["commit", "-q", "-m", "second"], "commit").unwrap();
        let (_, resolved) =
            resolve_repo_base_with_ref(&toplevel, Some("refs/heads/base-at-first")).unwrap();
        assert_eq!(resolved, first_commit);
        let (_, head) = resolve_repo_base_with_ref(&toplevel, None).unwrap();
        assert_ne!(head, first_commit, "HEAD moved past the base ref");

        let config = test_config(&dir);
        let base_dir = resolve_worktree_base_dir(&config, &toplevel).unwrap();
        let worktree = create_worktree(
            &toplevel,
            "",
            "runbase",
            0,
            &resolved,
            &base_dir,
            Some("worker"),
            &config,
        )
        .unwrap();
        let checked_out = run_git_checked(&worktree.path, &["rev-parse", "HEAD"], "rev-parse")
            .unwrap()
            .trim()
            .to_string();
        assert_eq!(checked_out, first_commit, "worktree branched from baseRef");
        assert!(!worktree.path.join("second.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn worktree_lifecycle_end_to_end() {
        if !git_available() {
            return; // CI without git skips the lifecycle assertions.
        }
        let (dir, toplevel, base_commit) = test_repo("lifecycle");
        let config = test_config(&dir);
        let base_dir = resolve_worktree_base_dir(&config, &toplevel).unwrap();
        let worktree = create_worktree(
            &toplevel,
            "",
            "run1",
            0,
            &base_commit,
            &base_dir,
            Some("worker"),
            &config,
        )
        .unwrap();
        assert!(worktree.path.join(".git").exists() || worktree.path.exists());
        assert_eq!(worktree.agent_cwd, worktree.path);

        // A write in the worktree does not pollute the main checkout.
        std::fs::write(worktree.path.join("feature.txt"), "change").unwrap();
        assert!(!toplevel.join("feature.txt").exists());

        let patch_dir = dir.join("patches");
        let diff = capture_worktree_diff(&worktree, "worker", &base_commit, &patch_dir).unwrap();
        assert_eq!(diff.files_changed, 1);
        assert_eq!(diff.insertions, 1);
        let patch_text = std::fs::read_to_string(&diff.patch_path).unwrap();
        assert!(patch_text.contains("feature.txt"));

        let manifest = write_handoff_manifest(
            &base_dir,
            "run1",
            "parallel",
            &toplevel,
            &base_commit,
            &[(
                0,
                "worker".to_string(),
                "complete".to_string(),
                diff.clone(),
            )],
            &[WorktreeCleanupTask::pending(
                0,
                &worktree.path,
                &worktree.branch,
            )],
        );
        assert!(manifest.exists());
        cleanup_worktree(
            &toplevel,
            &worktree,
            &base_commit,
            Some(&diff.patch_path),
            Some(&manifest),
        )
        .unwrap();
        assert!(!worktree.path.exists());
        // The second pass records the cleanup report (state complete).
        let manifest = write_handoff_manifest(
            &base_dir,
            "run1",
            "parallel",
            &toplevel,
            &base_commit,
            &[(0, "worker".to_string(), "complete".to_string(), diff)],
            &[WorktreeCleanupTask::removed(
                0,
                &worktree.path,
                &worktree.branch,
            )],
        );
        let raw = std::fs::read_to_string(&manifest).unwrap();
        let parsed: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["groups"][0]["cleanup"]["state"], json!("complete"));
        assert_eq!(
            parsed["groups"][0]["cleanup"]["tasks"][0]["worktreeRemoved"],
            json!(true)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod synthetic_path_tests {
    use super::*;

    fn worktree() -> (PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!(
            "rpi-sub-syn-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&base).unwrap();
        (base.clone(), base)
    }

    #[test]
    fn normalize_accepts_relative_paths_only() {
        let (base, _) = worktree();
        assert_eq!(
            normalize_synthetic_path(&base, "node_modules"),
            Some(base.join("node_modules"))
        );
        assert_eq!(
            normalize_synthetic_path(&base, "./build/out"),
            Some(base.join("build/out"))
        );
    }

    #[test]
    fn normalize_rejects_absolute_root_and_escapes() {
        let (base, _) = worktree();
        // Absolute paths replace the join base entirely (Path::join) — must
        // be rejected (upstream: "synthetic path must be relative").
        assert_eq!(normalize_synthetic_path(&base, "/etc"), None);
        #[cfg(windows)]
        assert_eq!(normalize_synthetic_path(&base, "C:\\Windows"), None);
        // Empty / whitespace-only.
        assert_eq!(normalize_synthetic_path(&base, ""), None);
        assert_eq!(normalize_synthetic_path(&base, "   "), None);
        // Worktree root itself.
        assert_eq!(normalize_synthetic_path(&base, "."), None);
        // Escapes via `..`.
        assert_eq!(normalize_synthetic_path(&base, ".."), None);
        assert_eq!(normalize_synthetic_path(&base, "../sibling"), None);
        assert_eq!(normalize_synthetic_path(&base, "a/../../escape"), None);
    }

    /// P1-3 regression: a hostile/buggy `worktreeSetupHook` stdout must not
    /// be able to delete directories outside the worktree.
    #[test]
    fn remove_synthetic_paths_never_escapes_the_worktree() {
        let (base, _) = worktree();
        // Sentinel OUTSIDE the worktree that a raw `join`-then-delete would
        // reach via the absolute path / `..` escape forms below.
        let outside = std::env::temp_dir().join(format!(
            "rpi-sub-syn-outside-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keep.txt"), "sentinel").unwrap();
        // Legitimate synthetic dir inside the worktree IS removed.
        std::fs::create_dir_all(base.join("build")).unwrap();
        std::fs::write(base.join("build/out.txt"), "x").unwrap();

        let info = WorktreeInfo {
            path: base.clone(),
            agent_cwd: base.clone(),
            branch: "b".to_string(),
            index: 0,
            node_modules_linked: false,
            synthetic_paths: vec![
                outside.to_string_lossy().into_owned(), // absolute → skip
                "../outside-escape".to_string(),        // escape → skip
                "build".to_string(),                    // legit → removed
            ],
        };
        remove_synthetic_paths(&info);

        assert!(
            outside.join("keep.txt").exists(),
            "sentinel outside the worktree must survive"
        );
        assert!(!base.join("build").exists(), "legit synthetic dir removed");
        let _ = std::fs::remove_dir_all(&outside);
        let _ = std::fs::remove_dir_all(&base);
    }
}
