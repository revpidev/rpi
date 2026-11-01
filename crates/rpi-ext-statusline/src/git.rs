//! Local Git probe for the workspace fields (TE52 R20 §4.2/§4.4).
//!
//! Only fields that need Git start a process: `project-name` resolves the
//! repository root (falling back to the cwd basename when the probe fails),
//! and `git-branch` reads the current branch. A detached HEAD, a non-repo
//! directory and a failed git binary all yield `None` — never a fabricated
//! short SHA.
//!
//! The refresh loop owns generation numbers: a probe started for an older
//! configuration/selection is discarded when it returns (`GitCache::apply`),
//! and the probe runs on the plugin runtime's blocking pool, never on the
//! host dispatch thread.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

/// One successful local Git probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitInfo {
    /// Repository top level (absolute), when git resolves it.
    pub root: Option<String>,
    /// Current branch; `None` for a detached HEAD or an unresolved branch.
    pub branch: Option<String>,
}

/// Probe `cwd` with the local git binary. `None` = not a repository, git is
/// unavailable, or the probe failed.
pub fn probe(cwd: &Path) -> Option<GitInfo> {
    let root = run_git(cwd, &["rev-parse", "--show-toplevel"])?;
    let root = root.trim();
    if root.is_empty() {
        return None;
    }
    let branch = run_git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])
        .map(|branch| branch.trim().to_owned())
        .filter(|branch| !branch.is_empty() && branch != "HEAD");
    Some(GitInfo {
        root: Some(root.to_owned()),
        branch,
    })
}

fn run_git(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// Cached probe result plus the generation used to discard late results.
#[derive(Debug, Default)]
pub struct GitCache {
    info: Option<GitInfo>,
    generation: u64,
    started: Option<Instant>,
    inflight: bool,
}

impl GitCache {
    pub fn info(&self) -> Option<&GitInfo> {
        self.info.as_ref()
    }

    /// Whether a probe should start now: no result yet, the previous one is
    /// older than `interval`, and nothing is in flight.
    pub fn should_fetch(&self, interval: Duration, now: Instant) -> bool {
        !self.inflight
            && self
                .started
                .is_none_or(|started| now.saturating_duration_since(started) >= interval)
    }

    /// Start a probe; returns the generation its result must carry.
    pub fn begin(&mut self, now: Instant) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.started = Some(now);
        self.inflight = true;
        self.generation
    }

    /// Invalidate any in-flight probe (selection changed/config removed) and
    /// clear the cached value.
    pub fn clear(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.info = None;
        self.started = None;
        self.inflight = false;
    }

    /// Apply a probe result. Returns whether the value was accepted (the
    /// generation must still be current) and changed.
    pub fn apply(&mut self, generation: u64, info: Option<GitInfo>) -> bool {
        if generation != self.generation {
            return false;
        }
        self.inflight = false;
        let changed = self.info != info;
        self.info = info;
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rpi-statusline-git-{label}-{}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
    }

    #[test]
    fn non_repository_probe_is_none() {
        let dir = temp_dir("plain");
        assert_eq!(probe(&dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn generation_gates_late_results() {
        let mut cache = GitCache::default();
        let first = cache.begin(Instant::now());
        let info = GitInfo {
            root: Some("/r".into()),
            branch: Some("main".into()),
        };
        assert!(cache.apply(first, Some(info.clone())));
        assert_eq!(cache.info(), Some(&info));
        // A stale generation is dropped.
        assert!(!cache.apply(first.wrapping_sub(1), None));
        assert_eq!(cache.info(), Some(&info));
        // Clear invalidates the in-flight probe.
        cache.begin(Instant::now());
        cache.clear();
        assert!(!cache.apply(first, None));
    }

    #[test]
    fn should_fetch_honours_the_interval_and_inflight_state() {
        let mut cache = GitCache::default();
        let now = Instant::now();
        assert!(cache.should_fetch(Duration::from_secs(5), now));
        let generation = cache.begin(now);
        assert!(!cache.should_fetch(Duration::from_secs(5), now));
        cache.apply(generation, None);
        assert!(!cache.should_fetch(Duration::from_secs(5), now));
        assert!(cache.should_fetch(Duration::from_secs(5), now + Duration::from_secs(5)));
    }

    #[test]
    fn branch_probe_reads_a_real_repository() {
        if !git_available() {
            // git is a development prerequisite for this test; make the
            // missing tool visible without failing unrelated environments.
            eprintln!("git is not installed; skipping the real-repository probe");
            return;
        }
        let dir = temp_dir("repo");
        let run = |args: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .output()
                .expect("git runs");
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        run(&["init"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test"]);
        std::fs::write(dir.join("file.txt"), b"x").expect("write");
        run(&["add", "."]);
        run(&["commit", "-m", "init"]);
        run(&["checkout", "-b", "te52"]);
        let info = probe(&dir).expect("repository probe");
        assert_eq!(info.branch.as_deref(), Some("te52"));
        assert_eq!(
            info.root.as_deref(),
            Some(dir.canonicalize().expect("canonical").to_str().unwrap())
        );
        // Detached HEAD: root stays, branch is omitted.
        run(&["checkout", "--detach"]);
        let detached = probe(&dir).expect("detached probe");
        assert_eq!(detached.branch, None);
        assert!(detached.root.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
