//! B13 missing-session-cwd entry matrix: print/RPC fail before any session is
//! created, while interactive Continue/Fork recover without rewriting the
//! original header (session-cwd.ts:14-59; main.ts:745-768).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use rpi::core::session_cwd::{MissingCwdDecision, get_missing_session_cwd_issue};
use rpi::core::session_manager::{ForkOptions, SessionManager};

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let dir =
            std::env::temp_dir().join(format!("rpi-session-cwd-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write_missing_cwd_session(dir: &Path, missing_cwd: &Path) -> PathBuf {
    let session_dir = dir.join("sessions");
    std::fs::create_dir_all(&session_dir).expect("sessions dir");
    let file = session_dir.join("2026-11-19T00-00-00-000Z_startup-cwd.jsonl");
    let header = serde_json::json!({
        "type": "session",
        "version": 3,
        "id": "startup-cwd",
        "timestamp": "2026-11-19T00:00:00.000Z",
        "cwd": missing_cwd.to_string_lossy(),
    });
    let message = serde_json::json!({
        "type": "message",
        "id": "m1",
        "parentId": null,
        "timestamp": "2026-11-19T00:00:01.000Z",
        "message": { "role": "user", "content": "hi" },
    });
    std::fs::write(&file, format!("{header}\n{message}\n")).expect("write session");
    file
}

fn command(binary: &str, dir: &Path) -> Command {
    let mut command = Command::new(binary);
    command
        .env("HOME", dir.join("home"))
        .env("RPI_CODING_AGENT_DIR", dir.join("agent"))
        .env("RPI_OFFLINE", "1")
        .env("RPI_SKIP_VERSION_CHECK", "1")
        .stdin(Stdio::null());
    command
}

#[test]
fn print_and_rpc_modes_fail_on_a_missing_session_cwd() {
    let dir = TempDir::new();
    let missing_cwd = dir.path().join("gone-project");
    let session_file = write_missing_cwd_session(dir.path(), &missing_cwd);

    for args in [
        vec![
            "--session".to_string(),
            session_file.to_string_lossy().into_owned(),
            "--print".to_string(),
            "hello".to_string(),
        ],
        vec![
            "--mode".to_string(),
            "rpc".to_string(),
            "--session".to_string(),
            session_file.to_string_lossy().into_owned(),
        ],
    ] {
        let output = command(env!("CARGO_BIN_EXE_rpi"), dir.path())
            .args(&args)
            .output()
            .expect("run rpi");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.code(),
            Some(1),
            "args={args:?}, stderr={stderr}"
        );
        assert!(
            stderr.contains("Stored session working directory does not exist"),
            "args={args:?}, stderr={stderr}"
        );
        assert!(
            stderr.contains("Session file:"),
            "args={args:?}, stderr={stderr}"
        );
    }
}

#[test]
fn interactive_continue_and_fork_recover_the_missing_cwd() {
    let dir = TempDir::new();
    let missing_cwd = dir.path().join("gone-project");
    let session_file = write_missing_cwd_session(dir.path(), &missing_cwd);
    let session_dir = dir.path().join("sessions");
    let fallback_cwd = dir.path().join("current-project");
    std::fs::create_dir_all(&fallback_cwd).expect("fallback cwd");

    // The app-level check sees the stored cwd and offers Continue/Fork.
    let opened = SessionManager::open(&session_file, Some(&session_dir), None).expect("open");
    let issue = get_missing_session_cwd_issue(&opened, &fallback_cwd).expect("missing cwd issue");
    assert_eq!(issue.session_cwd, missing_cwd.to_string_lossy());
    assert_eq!(issue.fallback_cwd, fallback_cwd.to_string_lossy());
    assert_eq!(
        issue.session_file.as_deref(),
        Some(session_file.to_string_lossy().as_ref())
    );
    drop(opened);

    // Continue: one-shot cwd override, same file, header not rewritten.
    let continued = SessionManager::open(&session_file, Some(&session_dir), Some(&fallback_cwd))
        .expect("continue");
    assert_eq!(continued.get_cwd(), fallback_cwd.as_path());
    assert_eq!(continued.get_session_file(), Some(session_file.as_path()));
    let header = std::fs::read_to_string(&session_file).expect("read header");
    assert!(
        header.contains(&missing_cwd.to_string_lossy().into_owned()),
        "Continue must not persist the cwd override: {header}"
    );
    drop(continued);

    // Fork: new session id/file under the current cwd; original kept.
    let forked = SessionManager::fork_from(
        &session_file,
        &fallback_cwd,
        Some(&session_dir),
        ForkOptions::default(),
    )
    .expect("fork");
    assert_eq!(forked.get_cwd(), fallback_cwd.as_path());
    assert_ne!(forked.get_session_id(), "startup-cwd");
    let forked_file = forked
        .get_session_file()
        .expect("forked file")
        .to_path_buf();
    assert_ne!(forked_file, session_file);
    assert!(forked_file.exists(), "forked file exists");
    assert!(session_file.exists(), "original is kept");
    let forked_header = std::fs::read_to_string(&forked_file).expect("read fork header");
    assert!(
        forked_header.contains(&fallback_cwd.to_string_lossy().into_owned()),
        "fork header records the current cwd: {forked_header}"
    );

    // Both decisions are the only options besides Cancel.
    let options = rpi::core::session_cwd::get_missing_session_cwd_options(&issue);
    assert_eq!(
        options
            .iter()
            .map(|option| option.decision)
            .collect::<Vec<_>>(),
        vec![
            Some(MissingCwdDecision::Continue),
            Some(MissingCwdDecision::Fork),
            None
        ]
    );
}
