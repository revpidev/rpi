//! Tests for the write tool (`crates/rpi/src/tools/write.rs`).
//!
//! Port of the write tool tests from `tools.test.ts:240-260`.

use rpi::tools::write::{create_write_tool, WriteToolOptions};
use rpi::tools::ToolContext;
use rpi_agent::error::AgentError;
use serde_json::json;
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;

fn make_ctx(cwd: &std::path::Path) -> ToolContext {
    ToolContext {
        cwd: cwd.to_path_buf(),
        session_env: None,
    }
}

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "rpi-write-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn test_write_file_success() {
    let tmp = TempDir::new();
    let ctx = make_ctx(tmp.path());
    let tool = create_write_tool(&ctx, WriteToolOptions::default());

    let result = tool
        .execute(
            "test-call-1",
            json!({"path": "output.txt", "content": "Hello, world!"}),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();

    let text = &result.content[0];
    match text {
        rpi_ai::types::ToolResultContent::Text(t) => {
            assert!(t.text.starts_with("Successfully wrote"));
            assert!(t.text.contains("output.txt"));
        }
        _ => panic!("expected text content"),
    }

    let content = std::fs::read_to_string(tmp.path().join("output.txt")).unwrap();
    assert_eq!(content, "Hello, world!");
}

#[tokio::test]
async fn test_write_byte_count_is_utf16() {
    let tmp = TempDir::new();
    let ctx = make_ctx(tmp.path());
    let tool = create_write_tool(&ctx, WriteToolOptions::default());

    // "héllo" = 5 UTF-16 code units (é is U+00E9, a single code unit)
    let result = tool
        .execute(
            "test-utf16",
            json!({"path": "out.txt", "content": "héllo"}),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();

    if let rpi_ai::types::ToolResultContent::Text(t) = &result.content[0] {
        // "héllo" has 5 UTF-16 code units
        assert!(t.text.contains("Successfully wrote 5 bytes to out.txt"));
    }
}

#[tokio::test]
async fn test_write_creates_parent_directories() {
    let tmp = TempDir::new();
    let ctx = make_ctx(tmp.path());
    let tool = create_write_tool(&ctx, WriteToolOptions::default());

    let result = tool
        .execute(
            "test-call-2",
            json!({"path": "a/b/c/deep.txt", "content": "deep"}),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();

    if let rpi_ai::types::ToolResultContent::Text(t) = &result.content[0] {
        assert!(t.text.contains("Successfully wrote"));
    }

    let content = std::fs::read_to_string(tmp.path().join("a/b/c/deep.txt")).unwrap();
    assert_eq!(content, "deep");
}

#[tokio::test]
async fn test_write_overwrites_existing() {
    let tmp = TempDir::new();
    let file = tmp.path().join("existing.txt");
    std::fs::write(&file, "old content").unwrap();

    let ctx = make_ctx(tmp.path());
    let tool = create_write_tool(&ctx, WriteToolOptions::default());

    tool.execute(
        "test-call-3",
        json!({"path": "existing.txt", "content": "new content"}),
        CancellationToken::new(),
        None,
    )
    .await
    .unwrap();

    let content = std::fs::read_to_string(&file).unwrap();
    assert_eq!(content, "new content");
}

#[tokio::test]
async fn test_write_aborted_before_start() {
    let tmp = TempDir::new();
    let ctx = make_ctx(tmp.path());
    let tool = create_write_tool(&ctx, WriteToolOptions::default());

    let token = tokio_util::sync::CancellationToken::new();
    token.cancel();

    let result = tool
        .execute(
            "test-abort",
            json!({"path": "abort.txt", "content": "data"}),
            token,
            None,
        )
        .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    if let AgentError::Message(msg) = err {
        assert_eq!(msg, "Operation aborted");
    } else {
        panic!("expected AgentError::Message, got {:?}", err);
    }

    // File should not exist.
    assert!(!tmp.path().join("abort.txt").exists());
}

#[tokio::test]
async fn test_write_empty_content() {
    let tmp = TempDir::new();
    let ctx = make_ctx(tmp.path());
    let tool = create_write_tool(&ctx, WriteToolOptions::default());

    let result = tool
        .execute(
            "test-empty",
            json!({"path": "empty.txt", "content": ""}),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();

    if let rpi_ai::types::ToolResultContent::Text(t) = &result.content[0] {
        assert!(t.text.contains("Successfully wrote 0 bytes"));
    }

    let content = std::fs::read_to_string(tmp.path().join("empty.txt")).unwrap();
    assert_eq!(content, "");
}

#[tokio::test]
async fn test_write_metadata() {
    let tmp = TempDir::new();
    let ctx = make_ctx(tmp.path());
    let tool = create_write_tool(&ctx, WriteToolOptions::default());

    assert_eq!(tool.name(), "write");
    assert_eq!(tool.label(), "write");
    assert!(tool.description().contains("Write content to a file"));

    let params = tool.parameters();
    assert!(params.get("properties").unwrap().get("path").is_some());
    assert!(params.get("properties").unwrap().get("content").is_some());
    let required = params.get("required").unwrap().as_array().unwrap();
    assert!(required.contains(&json!("path")));
    assert!(required.contains(&json!("content")));
}

#[tokio::test]
async fn test_write_and_edit_share_mutation_queue() {
    // Write and edit on the same file should serialize (no torn write).
    use rpi::tools::edit::{create_edit_tool, EditToolOptions};

    let tmp = TempDir::new();
    let file_path = tmp.path().join("shared.txt");
    std::fs::write(&file_path, "initial\n").unwrap();

    let ctx = make_ctx(tmp.path());

    // Spawn a write and an edit concurrently.
    let write_tool = create_write_tool(&ctx, WriteToolOptions::default());
    let edit_tool = create_edit_tool(&ctx, EditToolOptions::default());

    let write_fut = write_tool.execute(
        "w1",
        json!({"path": "shared.txt", "content": "from-write\n"}),
        CancellationToken::new(),
        None,
    );
    let edit_fut = edit_tool.execute(
        "e1",
        json!({"path": "shared.txt", "edits": [{"oldText": "initial\n", "newText": "from-edit\n"}]}),
        CancellationToken::new(),
        None,
    );

    // Race them. One will succeed in mutating the file first; the second
    // operates on whatever the first left. The key assertion is that neither
    // operation produces a corrupted file — the content is always valid UTF-8
    // matching one of the two operations' intent.
    let (write_result, edit_result) = tokio::join!(write_fut, edit_fut);

    // At least one should succeed; the other might fail (text not found after
    // write, or write happened first).
    // The final file content should be a clean string — not torn.
    let final_content = std::fs::read_to_string(&file_path).unwrap();
    assert!(
        final_content == "from-write\n" || final_content == "from-edit\n",
        "expected clean file, got: {final_content}"
    );

    // At least one succeeded.
    assert!(write_result.is_ok() || edit_result.is_ok());
}
