//! Port of the git-paths slice of
//! `packages/coding-agent/src/core/footer-data-provider.ts` @ pi 0.84.1+
//! (4181f66): `findGitPaths` (:16-48) and the `GitPaths` type (:6-11).
//!
//! Shared by the git-branch watcher (`modes/interactive/git_branch_watcher.rs`)
//! and the context-file shadow dedup (`core/system_prompt.rs`, commit
//! cced6a21d). The branch watcher previously carried a private copy that
//! dropped `common_git_dir`; this module restores it so the shadow logic can
//! compute `mainRepoRoot`.
//!
//! Intentional differences: none — the algorithm is a faithful port. The
//! watcher's own `resolve_path` helper is duplicated here as
//! [`resolve_join`] because it is trivial and keeps this module self-contained.

use std::path::{Path, PathBuf};

/// `GitPaths` (footer-data-provider.ts:6-11).
#[derive(Debug, Clone)]
pub struct GitPaths {
    /// The working-tree root that owns `.git` (`repoDir` upstream).
    pub repo_dir: PathBuf,
    /// The common git directory: the `.git` dir itself for a regular repo,
    /// or the resolved `commondir` target for a linked worktree. Used by
    /// `findShadowedContextFile` to compute `mainRepoRoot`.
    pub common_git_dir: PathBuf,
    /// `<gitDir>/HEAD`.
    pub head_path: PathBuf,
}

/// JS `path.resolve(base, segment)`: absolute segments win, otherwise join.
fn resolve_join(base: &Path, segment: &str) -> PathBuf {
    let segment = Path::new(segment);
    if segment.is_absolute() {
        segment.to_path_buf()
    } else {
        base.join(segment)
    }
}

/// `findGitPaths` (footer-data-provider.ts:16-48): walk up from `cwd`;
/// handles both regular repos (`.git` is a directory) and worktrees (`.git`
/// is a `gitdir: ` file). For linked worktrees, the `commondir` file inside
/// the per-worktree git dir is read to resolve the shared common git dir
/// (commit cced6a21d restored this field for the shadow dedup logic).
pub fn find_git_paths(cwd: &Path) -> Option<GitPaths> {
    let mut dir = Some(cwd);
    while let Some(current) = dir {
        let git_path = current.join(".git");
        if git_path.exists() {
            let metadata = std::fs::metadata(&git_path).ok()?;
            if metadata.is_file() {
                let content = std::fs::read_to_string(&git_path).ok()?;
                let content = content.trim();
                if let Some(gitdir) = content.strip_prefix("gitdir: ") {
                    let git_dir = resolve_join(current, gitdir.trim());
                    let head_path = git_dir.join("HEAD");
                    if !head_path.exists() {
                        return None;
                    }
                    // Read `commondir` (relative to git_dir) to resolve the
                    // shared common git dir (footer-data-provider.ts:29-32).
                    let common_dir_path = git_dir.join("commondir");
                    let common_git_dir = if common_dir_path.exists() {
                        match std::fs::read_to_string(&common_dir_path) {
                            Ok(raw) => resolve_join(&git_dir, raw.trim()),
                            Err(_) => git_dir.clone(),
                        }
                    } else {
                        git_dir.clone()
                    };
                    return Some(GitPaths {
                        repo_dir: current.to_path_buf(),
                        common_git_dir,
                        head_path,
                    });
                }
                // A `.git` file without the `gitdir: ` prefix is not a
                // worktree pointer: keep walking up.
            } else if metadata.is_dir() {
                let head_path = git_path.join("HEAD");
                if !head_path.exists() {
                    return None;
                }
                return Some(GitPaths {
                    repo_dir: current.to_path_buf(),
                    common_git_dir: git_path.clone(),
                    head_path,
                });
            }
        }
        dir = current.parent();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_repo(parent: &Path, name: &str, head: &str) -> PathBuf {
        let repo = parent.join(name);
        std::fs::create_dir_all(repo.join(".git")).expect("git dir");
        std::fs::write(repo.join(".git").join("HEAD"), head).expect("HEAD");
        repo
    }

    #[test]
    fn find_git_paths_regular_repo_common_equals_git() {
        let tmp =
            std::env::temp_dir().join(format!("rpi-git-paths-test-{}-regular", std::process::id()));
        std::fs::create_dir_all(&tmp).expect("temp");
        let repo = fake_repo(&tmp, "repo", "ref: refs/heads/main\n");
        let paths = find_git_paths(&repo).expect("found");
        assert_eq!(paths.repo_dir, repo);
        assert_eq!(paths.common_git_dir, repo.join(".git"));
        assert_eq!(paths.head_path, repo.join(".git").join("HEAD"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn find_git_paths_walks_up_from_nested_cwd() {
        let tmp =
            std::env::temp_dir().join(format!("rpi-git-paths-test-{}-nested", std::process::id()));
        std::fs::create_dir_all(&tmp).expect("temp");
        let repo = fake_repo(&tmp, "repo", "ref: refs/heads/main\n");
        let nested = repo.join("a").join("b");
        std::fs::create_dir_all(&nested).expect("nested");
        let paths = find_git_paths(&nested).expect("found from nested");
        assert_eq!(paths.repo_dir, repo);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn find_git_paths_worktree_resolves_common_dir() {
        let tmp = std::env::temp_dir().join(format!(
            "rpi-git-paths-test-{}-worktree",
            std::process::id()
        ));
        std::fs::create_dir_all(&tmp).expect("temp");

        // Simulate a linked worktree layout:
        //   main-repo/.git/             ← common git dir
        //   main-repo/.git/worktrees/wt/← per-worktree git dir
        //   main-repo/.git/worktrees/wt/commondir → ../../../
        //   main-repo/wt/.git           → gitdir: …worktrees/wt
        let main_repo = tmp.join("main-repo");
        let common_git = main_repo.join(".git");
        let wt_git_dir = common_git.join("worktrees").join("wt");
        std::fs::create_dir_all(&wt_git_dir).expect("wt git dir");
        std::fs::write(wt_git_dir.join("HEAD"), "ref: refs/heads/feat\n").expect("HEAD");
        // commondir is relative to the per-worktree git dir. Real git writes
        // `../..` from `.git/worktrees/wt/` → resolves to `.git/`.
        std::fs::write(wt_git_dir.join("commondir"), "../..\n").expect("commondir");

        let worktree_root = main_repo.join("wt");
        std::fs::create_dir_all(&worktree_root).expect("worktree root");
        std::fs::write(
            worktree_root.join(".git"),
            format!("gitdir: {}\n", wt_git_dir.display()),
        )
        .expect("gitfile");

        let paths = find_git_paths(&worktree_root).expect("worktree resolved");
        assert_eq!(paths.repo_dir, worktree_root);
        assert_eq!(paths.head_path, wt_git_dir.join("HEAD"));
        // common_git_dir should resolve to the main .git directory. The
        // resolved path contains `..` components which canonicalize resolves.
        let canonical_common = std::fs::canonicalize(&paths.common_git_dir)
            .unwrap_or_else(|_| paths.common_git_dir.clone());
        let canonical_expected =
            std::fs::canonicalize(&common_git).unwrap_or_else(|_| common_git.clone());
        assert_eq!(canonical_common, canonical_expected);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn find_git_paths_returns_none_outside_repos() {
        let tmp =
            std::env::temp_dir().join(format!("rpi-git-paths-test-{}-none", std::process::id()));
        std::fs::create_dir_all(&tmp).expect("temp");
        assert!(find_git_paths(&tmp).is_none());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
