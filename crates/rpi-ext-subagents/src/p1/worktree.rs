//! Managed git worktree isolation for writing children (FR-P1-06).
//!
//! Port of pi-subagents `src/runs/shared/worktree.ts` @ v0.48.0 (56f97234):
//! branch from clean HEAD (`git worktree add <base>/rpi-worktree-<runId>-<n>
//! -b rpi-parallel-<runId>-<n> HEAD`), agent cwd = worktree + repo-relative
//! prefix, node_modules symlink + setup hook synthetic paths, patch capture
//! (`git add -A` + `diff --cached <baseCommit>`), handoff manifest
//! (`handoffs/<runId>.json`, parallel-handoff.ts shape), and rollback-safe
//! cleanup. git runs through direct `std::process::Command` spawns — same
//! orchestration style as the rpi child processes (design §1.1; the host
//! `exec` envelope is synchronous and cannot serve the async runner —
//! deviation registered in TE05).
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
/// setup hook, with rollback on setup failure (worktree remove --force +
/// branch -D, best effort preserving the original error).
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
            "HEAD",
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
    let branch_for_cleanup = branch.clone();
    let result = (|| -> Result<WorktreeInfo, String> {
        let node_modules_linked = link_node_modules_if_present(toplevel, &worktree_path);
        if node_modules_linked {
            synthetic_paths.push("node_modules".to_string());
        }
        if let Some((hook, timeout_ms)) = config.worktree_setup_hook() {
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
            let _ = run_git(
                toplevel,
                &[
                    "worktree",
                    "remove",
                    "--force",
                    &worktree_path.to_string_lossy(),
                ],
            );
            let _ = run_git(toplevel, &["branch", "-D", &branch_for_cleanup]);
            Err(error)
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
        false
    }
}

/// `runWorktreeSetupHook` (worktree.ts:336-379): stdin = the v1 payload,
/// stdout = JSON object `{ syntheticPaths?: string[] }`; non-zero exit,
/// malformed stdout, or timeout (default 30s) fail the setup.
#[allow(clippy::too_many_arguments)]
fn run_worktree_setup_hook(
    hook: &str,
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
        "version": 1,
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
    // on its own thread, the watchdog polls it.
    let mut child = child;
    if let Some(mut stdin_handle) = child.stdin.take() {
        use std::io::Write;
        let _ = stdin_handle.write_all(stdin.as_bytes());
    }
    let waiter = std::thread::spawn(move || child.wait_with_output());
    let output = wait_with_timeout(waiter, std::time::Duration::from_millis(timeout_ms))
        .map_err(|_| format!("worktree setup hook timed out after {timeout_ms}ms"))?;
    if !output.status.success() {
        return Err(format!(
            "worktree setup hook failed (exit {:?}): {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
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
) -> std::io::Result<std::process::Output> {
    // The watchdog polls the join handle; on timeout the hook process was
    // already detached by taking stdin/stdout — its exit is not waited on.
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if waiter.is_finished() {
            return waiter
                .join()
                .map_err(|_| std::io::Error::other("hook thread panicked"))?;
        }
        if std::time::Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "hook timeout",
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// `normalizeSyntheticPath` (worktree.ts:297-315): relative only, no escapes,
/// not repo root; synthetic paths are unlinked before diffs/cleanup.
fn remove_synthetic_paths(worktree: &WorktreeInfo) {
    for raw in &worktree.synthetic_paths {
        let path = worktree.path.join(raw);
        if path == worktree.path {
            continue;
        }
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

/// `captureWorktreeDiff` (worktree.ts:509-537): add -A, three diff flavors
/// against the base commit, patch file written next to the manifest.
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
        &["diff", "--cached", base_commit],
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

/// `writeParallelHandoffGroup` (parallel-handoff.ts:74-160) — the manifest
/// survives cleanup so the orchestrator can apply/inspect patches later.
pub fn write_handoff_manifest(
    base_dir: &Path,
    run_id: &str,
    mode: &str,
    cwd: &Path,
    base_commit: &str,
    children: &[(usize, String, String, WorktreeDiff)],
) -> PathBuf {
    let handoff_dir = base_dir.join("handoffs");
    let _ = std::fs::create_dir_all(&handoff_dir);
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
            "cleanup": { "state": "complete", "tasks": [], "pruned": [] },
        }],
    });
    let path = handoff_dir.join(format!("{run_id}.json"));
    let _ = crate::artifacts::write_metadata(&path, &manifest);
    path
}

/// `cleanupSingleWorktree` (worktree.ts:566-661) P1 form: uncommitted residue
/// blocks cleanup unless the patch was journaled in the handoff manifest
/// (`handoffRecordsPatch`); synthetic paths removed first.
pub fn cleanup_worktree(
    toplevel: &Path,
    worktree: &WorktreeInfo,
    manifest_path: Option<&Path>,
) -> Result<(), String> {
    remove_synthetic_paths(worktree);
    let dirty = run_git(&worktree.path, &["status", "--porcelain"])
        .map(|output| {
            output.status.success() && !String::from_utf8_lossy(&output.stdout).trim().is_empty()
        })
        .unwrap_or(false);
    if dirty {
        let recorded = manifest_path.is_some_and(|path| path.exists());
        if !recorded {
            return Err(format!(
                "worktree {} has uncommitted changes and no handoff manifest recorded its patch; kept for inspection",
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

/// Resolve the repo toplevel + base commit for a run ("clean HEAD" means the
/// current HEAD; upstream snapshots it as `baseCommit` before branching).
pub fn resolve_repo_base(cwd: &Path) -> Result<(PathBuf, String), String> {
    let toplevel = run_git_checked(
        cwd,
        &["rev-parse", "--show-toplevel"],
        "git rev-parse --show-toplevel",
    )?
    .trim()
    .to_string();
    let head = run_git_checked(cwd, &["rev-parse", "HEAD"], "git rev-parse HEAD")?
        .trim()
        .to_string();
    Ok((PathBuf::from(toplevel), head))
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
    fn worktree_lifecycle_end_to_end() {
        if !git_available() {
            return; // CI without git skips the lifecycle assertions.
        }
        let dir = std::env::temp_dir().join(format!("rpi-sub-wt-{}", std::process::id()));
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

        let config = ExtensionConfig::default();
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
        assert!(!repo.join("feature.txt").exists());

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
            &[(0, "worker".to_string(), "complete".to_string(), diff)],
        );
        assert!(manifest.exists());
        cleanup_worktree(&toplevel, &worktree, Some(&manifest)).unwrap();
        assert!(!worktree.path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
