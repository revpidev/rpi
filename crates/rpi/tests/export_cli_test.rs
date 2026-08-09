//! `rpi --export <file> [output.html]` CLI contract test (main.ts:526-538):
//! export a session JSONL to HTML and exit, before any mode starts.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "rpi-export-cli-test-{}-{nanos}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
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

const SESSION_JSONL: &str = concat!(
    "{\"type\":\"session\",\"version\":3,\"id\":\"sess-cli\",\"timestamp\":\"2025-01-01T00:00:00.000Z\",\"cwd\":\"/tmp\"}\n",
    "{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"2025-01-01T00:00:00.000Z\",\"message\":{\"role\":\"user\",\"content\":\"cli export\",\"timestamp\":1}}\n",
);

fn run_export(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_rpi"))
        .args(args)
        .current_dir(dir)
        // No network / no real HOME: the export path exits before startup.
        .env("RPI_OFFLINE", "1")
        .output()
        .expect("spawn rpi")
}

#[test]
fn export_writes_html_and_prints_path() {
    let tmp = TempDir::new();
    std::fs::write(tmp.path().join("s.jsonl"), SESSION_JSONL).expect("write session");
    let output = run_export(tmp.path(), &["--export", "s.jsonl", "out.html"]);
    assert_eq!(output.status.code(), Some(0), "stderr: {:?}", output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Exported to: out.html"), "stdout: {stdout}");
    let html = std::fs::read_to_string(tmp.path().join("out.html")).expect("read html");
    assert!(html.starts_with("<!DOCTYPE html>"), "template skeleton");
    assert!(
        !html.contains("{{SESSION_DATA}}"),
        "placeholders substituted"
    );
}

#[test]
fn export_default_output_name() {
    let tmp = TempDir::new();
    std::fs::write(tmp.path().join("abc.jsonl"), SESSION_JSONL).expect("write session");
    let output = run_export(tmp.path(), &["--export", "abc.jsonl"]);
    assert_eq!(output.status.code(), Some(0), "stderr: {:?}", output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Exported to: rpi-session-abc.html"),
        "stdout: {stdout}"
    );
    assert!(tmp.path().join("rpi-session-abc.html").exists());
}

#[test]
fn export_missing_file_exits_1() {
    let tmp = TempDir::new();
    let output = run_export(tmp.path(), &["--export", "nope.jsonl"]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Error: File not found:"),
        "stderr: {stderr}"
    );
}
