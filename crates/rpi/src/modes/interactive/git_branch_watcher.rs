//! Git branch watcher for the footer — the git slice of
//! `core/footer-data-provider.ts` @ pi 0.84.1+ (4181f66): `resolveBranchWithGitSync`
//! (51-59), `resolveGitBranchSync` (239-251) and `setupGitWatcher` (307-381).
//! `findGitPaths` and `GitPaths` are shared from `core/git_paths.rs`.
//!
//! The watcher is a polling thread (the `theme_watcher.rs` pattern): each
//! tick it re-resolves the branch for the `FooterDataProvider`'s current cwd
//! — so session rebinds (`set_cwd`) are followed automatically — writes the
//! provider slot on change and queues [`UiCommand::GitBranchChanged`] for the
//! drain, which invalidates the footer (the upstream `onBranchChange`
//! subscriber, interactive-mode.ts:807-809). The thread never locks a
//! component itself (lock contract).
//!
//! Intentional differences:
//! - A 100ms content poll replaces `fs.watch` on HEAD's parent directory +
//!   the 500ms debounce (footer-data-provider.ts:101, 313-324): no `notify`
//!   dependency, and comparing the resolved branch makes git's atomic
//!   write-temp-rename-over-HEAD (the upstream reason for watching the
//!   directory) a non-issue. The poll interval is injectable for tests.
//! - The reftable watchers (footer-data-provider.ts:342-380) are not ported:
//!   branch switches rewrite the HEAD symref in reftable repos too, so
//!   polling HEAD covers the footer (which only shows the branch name).
//! - The WSL `watchFile` fallback (`shouldPollGitHead`,
//!   footer-data-provider.ts:83-93, 325-337) is not ported: polling is the
//!   only mechanism here.
//! - Only the synchronous resolve exists; the async variant
//!   (`resolveGitBranchAsync`, footer-data-provider.ts:253-267) is the same
//!   logic driven by the debounce timer upstream.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::core::git_paths::find_git_paths;
use crate::core::git_paths::GitPaths;
use crate::modes::interactive::interactive_mode::{InteractiveUi, UiCommand};

/// Poll interval for the git branch watcher. Upstream debounces `fs.watch`
/// events by 500ms (footer-data-provider.ts:101); a 100ms poll of a tiny
/// file is cheaper than that latency suggests.
pub(crate) const GIT_WATCH_POLL_INTERVAL: Duration = Duration::from_millis(100);

// `GitPaths` and `find_git_paths` now live in `core/git_paths.rs` (restored
// `common_git_dir` for the context-file shadow dedup, commit cced6a21d).

/// `resolveBranchWithGitSync` (footer-data-provider.ts:51-59): ask git for
/// the current branch; `None` on detached HEAD or when git is unavailable.
/// Only used for the rare `.invalid` HEAD symref.
fn resolve_branch_with_git(repo_dir: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args([
            "--no-optional-locks",
            "symbolic-ref",
            "--quiet",
            "--short",
            "HEAD",
        ])
        .current_dir(repo_dir)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if branch.is_empty() {
        None
    } else {
        Some(branch)
    }
}

/// `resolveGitBranchSync` (footer-data-provider.ts:239-251): read the HEAD
/// symref; plain (non-symref) HEAD is `"detached"`, an unreadable HEAD is
/// `None` (the upstream catch → null).
fn resolve_git_branch(paths: &GitPaths) -> Option<String> {
    let content = std::fs::read_to_string(&paths.head_path).ok()?;
    let content = content.trim();
    match content.strip_prefix("ref: refs/heads/") {
        Some(".invalid") => {
            Some(resolve_branch_with_git(&paths.repo_dir).unwrap_or_else(|| "detached".to_string()))
        }
        Some(branch) => Some(branch.to_string()),
        None => Some("detached".to_string()),
    }
}

/// Resolve the current branch for a working directory: `None` outside a
/// repository, `Some("detached")` on a detached HEAD.
pub(crate) fn resolve_branch_for_cwd(cwd: &Path) -> Option<String> {
    find_git_paths(cwd).as_ref().and_then(resolve_git_branch)
}

/// The watcher's per-cwd cache: `findGitPaths` is re-run only when the
/// provider's cwd changes (session rebind), mirroring the upstream
/// `setCwd` → `findGitPaths` reset (footer-data-provider.ts:169-184).
#[derive(Default)]
struct GitWatchState {
    cwd: Option<PathBuf>,
    paths: Option<GitPaths>,
}

/// One watcher tick: re-resolve the branch and, on change, update the
/// provider and queue the footer invalidation. Returns the resolved branch
/// (test hook).
fn poll_git_branch(ui_state: &InteractiveUi, state: &mut GitWatchState) -> Option<String> {
    let cwd = ui_state.footer_data.cwd();
    if state.cwd.as_deref() != Some(cwd.as_path()) {
        state.paths = find_git_paths(&cwd);
        state.cwd = Some(cwd);
    }
    let branch = state.paths.as_ref().and_then(resolve_git_branch);
    if ui_state.footer_data.get_git_branch() != branch {
        ui_state.footer_data.set_git_branch(branch.clone());
        ui_state.push(UiCommand::GitBranchChanged);
        ui_state.render_handle.request_render();
    }
    branch
}

/// `setupGitWatcher` (footer-data-provider.ts:307-381) as a polling thread.
/// Stop by setting the shared `stop` flag; the caller joins the handle.
pub(crate) fn spawn_git_branch_watcher(
    ui_state: Arc<InteractiveUi>,
    stop: Arc<AtomicBool>,
    interval: Duration,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("rpi-git-watcher".to_string())
        .spawn(move || {
            let mut state = GitWatchState::default();
            while !stop.load(Ordering::Relaxed) {
                poll_git_branch(&ui_state, &mut state);
                std::thread::sleep(interval);
            }
        })
        .expect("spawn git branch watcher thread")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modes::interactive::interactive_mode::{InteractiveMode, InteractiveModeOptions};
    use crate::modes::interactive::test_support::{build_test_session, TempDir, TestTerminal};

    /// A fake repository: `<dir>/.git/HEAD` with the given HEAD content.
    fn fake_repo(parent: &Path, name: &str, head: &str) -> PathBuf {
        let repo = parent.join(name);
        std::fs::create_dir_all(repo.join(".git")).expect("git dir");
        std::fs::write(repo.join(".git").join("HEAD"), head).expect("HEAD");
        repo
    }

    #[test]
    fn find_git_paths_walks_up_from_nested_cwd() {
        let tmp = TempDir::new();
        let repo = fake_repo(tmp.path(), "repo", "ref: refs/heads/main\n");
        let nested = repo.join("a").join("b");
        std::fs::create_dir_all(&nested).expect("nested dir");
        let paths = find_git_paths(&nested).expect("repo found from nested cwd");
        assert_eq!(paths.repo_dir, repo);
        assert_eq!(paths.head_path, paths.repo_dir.join(".git").join("HEAD"));
    }

    #[test]
    fn find_git_paths_handles_worktree_gitfile() {
        let tmp = TempDir::new();
        // The real git dir lives elsewhere; the worktree carries a pointer.
        let git_dir = tmp.path().join("main-checkout").join(".git");
        std::fs::create_dir_all(&git_dir).expect("git dir");
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").expect("HEAD");
        let worktree = tmp.path().join("worktree");
        std::fs::create_dir_all(&worktree).expect("worktree dir");
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", git_dir.display()),
        )
        .expect("gitfile");
        let paths = find_git_paths(&worktree).expect("worktree resolved");
        assert_eq!(paths.repo_dir, worktree);
        assert_eq!(paths.head_path, git_dir.join("HEAD"));
    }

    #[test]
    fn find_git_paths_requires_head() {
        let tmp = TempDir::new();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).expect("git dir without HEAD");
        assert!(find_git_paths(&repo).is_none());
    }

    #[test]
    fn find_git_paths_returns_none_outside_repos() {
        let tmp = TempDir::new();
        assert!(find_git_paths(tmp.path()).is_none());
    }

    #[test]
    fn resolve_git_branch_reads_symref_detached_and_missing() {
        let tmp = TempDir::new();
        let repo = fake_repo(tmp.path(), "repo", "ref: refs/heads/feature/x\n");
        let paths = find_git_paths(&repo).expect("paths");
        assert_eq!(resolve_git_branch(&paths), Some("feature/x".to_string()));

        // Detached HEAD: plain commit-ish content.
        std::fs::write(
            repo.join(".git").join("HEAD"),
            "2efa728d2ee90ef597626e96b1e28ef2b279f07c\n",
        )
        .expect("detached HEAD");
        assert_eq!(resolve_git_branch(&paths), Some("detached".to_string()));

        // Unreadable HEAD → None (upstream catch → null).
        std::fs::remove_file(repo.join(".git").join("HEAD")).expect("remove HEAD");
        assert_eq!(resolve_git_branch(&paths), None);
    }

    #[test]
    fn resolve_git_branch_invalid_symref_falls_back_to_detached() {
        let tmp = TempDir::new();
        // `.invalid` with no real git repo underneath: the git subprocess
        // fails and the branch degrades to "detached"
        // (footer-data-provider.ts:245).
        let repo = fake_repo(tmp.path(), "repo", "ref: refs/heads/.invalid\n");
        let paths = find_git_paths(&repo).expect("paths");
        assert_eq!(resolve_git_branch(&paths), Some("detached".to_string()));
    }

    /// Build a mode over the test session (the watcher polls its
    /// `FooterDataProvider`); the returned `TempDir` keeps the session's
    /// agent dir alive and hosts the fake repos.
    async fn mode_ui() -> (InteractiveMode, TempDir) {
        let harness = build_test_session().await;
        let crate::modes::interactive::test_support::TestSession { _tmp, runtime, .. } = harness;
        let mode = InteractiveMode::with_terminal(
            runtime,
            InteractiveModeOptions::default(),
            Box::new(TestTerminal::new()),
        );
        (mode, _tmp)
    }

    #[tokio::test]
    async fn poll_updates_branch_and_queues_footer_invalidation() {
        let (mode, tmp) = mode_ui().await;
        let ui = &mode.ui_state;
        let repo = fake_repo(tmp.path(), "repo", "ref: refs/heads/main\n");
        ui.footer_data.set_cwd(&repo);

        let mut state = GitWatchState::default();
        let branch = poll_git_branch(ui, &mut state);
        assert_eq!(branch, Some("main".to_string()));
        assert_eq!(ui.footer_data.get_git_branch(), Some("main".to_string()));
        ui.drain_events();

        // A branch switch (HEAD rewrite) is picked up on the next tick.
        std::fs::write(repo.join(".git").join("HEAD"), "ref: refs/heads/topic\n")
            .expect("switch branch");
        let branch = poll_git_branch(ui, &mut state);
        assert_eq!(branch, Some("topic".to_string()));
        assert_eq!(ui.footer_data.get_git_branch(), Some("topic".to_string()));

        // Leaving the repository clears the branch.
        ui.footer_data.set_cwd(tmp.path());
        let branch = poll_git_branch(ui, &mut state);
        assert_eq!(branch, None);
        assert_eq!(ui.footer_data.get_git_branch(), None);
    }

    #[tokio::test]
    async fn watcher_thread_follows_head_and_cwd_changes() {
        let (mode, tmp) = mode_ui().await;
        let ui = mode.ui_state.clone();
        let repo_a = fake_repo(tmp.path(), "a", "ref: refs/heads/main\n");
        let repo_b = fake_repo(tmp.path(), "b", "ref: refs/heads/other\n");
        ui.footer_data.set_cwd(&repo_a);

        let stop = Arc::new(AtomicBool::new(false));
        let handle = spawn_git_branch_watcher(
            Arc::clone(&ui),
            Arc::clone(&stop),
            Duration::from_millis(10),
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let wait_for = |ui: &Arc<InteractiveUi>, expected: Option<String>| {
            while std::time::Instant::now() < deadline {
                if ui.footer_data.get_git_branch() == expected {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            false
        };
        assert!(wait_for(&ui, Some("main".to_string())), "initial branch");

        std::fs::write(
            repo_a.join(".git").join("HEAD"),
            "ref: refs/heads/switched\n",
        )
        .expect("switch branch");
        assert!(wait_for(&ui, Some("switched".to_string())), "HEAD change");

        // Session rebind repoints the provider's cwd; the watcher follows.
        ui.footer_data.set_cwd(&repo_b);
        assert!(wait_for(&ui, Some("other".to_string())), "cwd change");

        stop.store(true, Ordering::Relaxed);
        handle.join().expect("watcher joins");
    }
}
