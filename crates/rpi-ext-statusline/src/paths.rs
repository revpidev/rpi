//! Path derivation: agent dir, session-dir layout, newest session file.
//!
//! The ABI has no accessor for the current session file or session id
//! (TE12 "rpi 地基" verification), so — like subagents TE-D16 — this module
//! derives them from the host's deterministic layout
//! (`rpi/src/core/session_manager.rs:1045-1073`):
//! `<agentDir>/sessions/--<encoded cwd>--/<ISO ts>_<uuidv7>.jsonl`.
//! Known precision limit (TE-D34): a fresh subagents child writing into the
//! same cwd directory can win the mtime race; mitigated by the sticky latch
//! in `state.rs` plus the stem-shape filter below.

use std::path::{Component, Path, PathBuf};

/// `RPI_CODING_AGENT_DIR` (ADR-0001 `PI_*` → `RPI_*` rename).
pub const ENV_AGENT_DIR: &str = "RPI_CODING_AGENT_DIR";
const ENV_SESSION_DIR: &str = "RPI_CODING_AGENT_SESSION_DIR";
const CONFIG_DIR_NAME: &str = ".rpi";

/// Home directory from HOME/USERPROFILE (subagents paths.rs:75-84 form; no
/// passwd fallback).
pub fn home_dir() -> Option<PathBuf> {
    for key in ["HOME", "USERPROFILE"] {
        if let Some(value) = std::env::var_os(key) {
            if !value.is_empty() {
                return Some(PathBuf::from(value));
            }
        }
    }
    None
}

/// Lexically absolutize: expand a leading `~`, then collapse `.`/`..`
/// segments without touching symlinks (subagents paths.rs `normalizePath`).
fn normalize_path(input: &str) -> PathBuf {
    let expanded: PathBuf = if let Some(rest) = input.strip_prefix("~/") {
        match home_dir() {
            Some(home) => home.join(rest),
            None => PathBuf::from(rest),
        }
    } else {
        PathBuf::from(input)
    };
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(expanded)
    };
    let mut out = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// `getAgentDir` equivalent (rpi config.rs:111-121): `RPI_CODING_AGENT_DIR`
/// env override, else `~/.rpi/agent`.
pub fn get_agent_dir() -> PathBuf {
    if let Some(env_dir) = std::env::var_os(ENV_AGENT_DIR) {
        if !env_dir.is_empty() {
            return normalize_path(&env_dir.to_string_lossy());
        }
    }
    match home_dir() {
        Some(home) => home.join(CONFIG_DIR_NAME).join("agent"),
        None => PathBuf::from(CONFIG_DIR_NAME).join("agent"),
    }
}

/// Encode a resolved cwd into the session subdirectory name
/// (`--<cwd>--` rule, rpi config.rs:150-162 `encode_cwd_dir_name`): strip
/// one leading `/` or `\`, then replace every `/`, `\`, `:` with `-`.
pub fn encode_cwd_dir_name(resolved_cwd: &str) -> String {
    let stripped = resolved_cwd
        .strip_prefix(['/', '\\'])
        .unwrap_or(resolved_cwd);
    let safe: String = stripped
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' => '-',
            _ => c,
        })
        .collect();
    format!("--{safe}--")
}

/// Expand a configured `settings.sessionDir` value (subagents
/// `expand_tilde_and_resolve`): `~` expansion, relative paths resolve
/// against the process cwd.
fn expand_tilde_and_resolve(configured: &str) -> PathBuf {
    normalize_path(configured)
}

/// Resolve the session storage directory the host would use for `cwd`
/// (subagents paths.rs:234-246 `resolve_parent_session_dir`):
/// `RPI_CODING_AGENT_SESSION_DIR` env > `settings.sessionDir` >
/// `<agentDir>/sessions/--<encoded cwd>--`.
pub fn resolve_session_dir(cwd: &Path, settings_session_dir: Option<&str>) -> PathBuf {
    if let Some(env_dir) = std::env::var_os(ENV_SESSION_DIR) {
        if !env_dir.is_empty() {
            return normalize_path(&env_dir.to_string_lossy());
        }
    }
    if let Some(configured) = settings_session_dir.filter(|s| !s.is_empty()) {
        return expand_tilde_and_resolve(configured);
    }
    get_agent_dir()
        .join("sessions")
        .join(encode_cwd_dir_name(&cwd.to_string_lossy()))
}

/// Whether a file stem matches the session-file shape
/// `<ISO ts>_<uuidv7>` (session_manager.rs:1045-1067). Filters out
/// subagents fork artifacts (`fork.jsonl`, `fork-N.jsonl`) and stray files
/// (TE-D34 mitigation).
fn is_session_file_stem(stem: &str) -> bool {
    if stem.starts_with("fork") {
        return false;
    }
    let Some((_, tail)) = stem.rsplit_once('_') else {
        return false;
    };
    // uuidv7: 36 chars, 8-4-4-4-12 hex groups.
    tail.len() == 36 && tail.matches('-').count() == 4 && {
        let groups: Vec<&str> = tail.split('-').collect();
        groups.len() == 5
            && [8usize, 4, 4, 4, 12]
                .iter()
                .zip(&groups)
                .all(|(want, group)| {
                    group.len() == *want && group.chars().all(|c| c.is_ascii_hexdigit())
                })
    }
}

/// Newest shape-valid `*.jsonl` in `dir` by mtime (subagents
/// `findLatestSessionFile` paths.rs:250-273 plus the TE-D34 stem filter);
/// `None` when the directory has none.
pub fn find_latest_session_file(dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        if !path
            .file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(is_session_file_stem)
        {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let Ok(modified) = meta.modified() else {
            continue;
        };
        if best
            .as_ref()
            .is_none_or(|(best_time, _)| modified > *best_time)
        {
            best = Some((modified, path));
        }
    }
    best.map(|(_, path)| path)
}

/// Session id = the uuidv7 tail of the session-file stem
/// (`session_manager.rs:314` `create_session_id = uuidv7()`).
pub fn session_id_from_path(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let (_, tail) = stem.rsplit_once('_')?;
    is_session_file_stem(stem).then(|| tail.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_cwd_dir_name_strips_leading_separator() {
        assert_eq!(encode_cwd_dir_name("/home/leven/dev"), "--home-leven-dev--");
        // `:` and `\` each map to `-` (per-character, host config.rs:150-162).
        assert_eq!(encode_cwd_dir_name("C:\\work"), "--C--work--");
        assert_eq!(encode_cwd_dir_name("/"), "----");
    }

    #[test]
    fn session_file_stem_shape() {
        let good = "2026-08-19T10-00-00-000_018f6a1e-4c3b-7abc-8d2e-9f0a1b2c3d4e";
        assert!(is_session_file_stem(good));
        // Fork artifacts (subagents) and stray names are rejected.
        assert!(!is_session_file_stem("fork"));
        assert!(!is_session_file_stem("fork-2"));
        assert!(!is_session_file_stem("notes"));
        assert!(!is_session_file_stem("2026-08-19_not-a-uuid"));
        // Wrong uuid group lengths.
        assert!(!is_session_file_stem(
            "2026-08-19_018f6a1e4c3b7abc8d2e9f0a1b2c3d4effff"
        ));
    }

    #[test]
    fn session_id_extraction() {
        let path = Path::new(
            "/tmp/sessions/--x--/2026-08-19T10-00-00-000_018f6a1e-4c3b-7abc-8d2e-9f0a1b2c3d4e.jsonl",
        );
        assert_eq!(
            session_id_from_path(path).as_deref(),
            Some("018f6a1e-4c3b-7abc-8d2e-9f0a1b2c3d4e")
        );
        assert_eq!(session_id_from_path(Path::new("/tmp/fork.jsonl")), None);
    }

    #[test]
    fn find_latest_session_file_filters_by_shape_and_mtime() {
        let dir = std::env::temp_dir().join(format!("rpi-statusline-paths-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let newest = dir.join("2026-08-19T11-00-00-000_018f6a1e-4c3b-7abc-8d2e-9f0a1b2c3d4e.jsonl");
        let older = dir.join("2026-08-19T10-00-00-000_11111111-2222-3333-4444-555555555555.jsonl");
        let fork = dir.join("fork-3.jsonl");
        std::fs::write(&older, b"{}").expect("write");
        std::fs::write(&fork, b"{}").expect("write");
        // Ensure mtime(newest) > mtime(older): write newest last after a
        // small delay (same-timestamp filesystems would tie).
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&newest, b"{}").expect("write");
        assert_eq!(
            find_latest_session_file(&dir),
            Some(newest.clone()),
            "newest shape-valid file wins; fork artifacts skipped"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
