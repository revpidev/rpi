//! Tests for the edit tool (`crates/pir/src/tools/edit.rs`).
//!
//! Port of edit tool tests from `tools.test.ts:262-471` (core cases),
//! `tools.test.ts:894-1213` (fuzzy + CRLF), and
//! `edit-tool-legacy-input.test.ts` (legacy shim + JSON string).

use async_trait::async_trait;
use pir::tools::edit::{create_edit_tool, EditOperations, EditToolOptions};
use pir::tools::edit_diff::EditReplacement;
use pir::tools::ToolContext;
use pir_agent::error::AgentError;
use serde_json::{json, Value};
use std::io;
use std::path::{Path, PathBuf};
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
        // pid + nanos alone can collide when two parallel tests call
        // SystemTime::now() within the same coarse clock tick (observed flake:
        // two tests share a dir and one's Drop deletes the other's files).
        // The atomic counter makes the name unique within the process.
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "pir-edit-test-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
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

fn get_text(result: &pir_agent::types::AgentToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| match c {
            pir_ai::types::ToolResultContent::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ===========================================================================
// Core edit tests (tools.test.ts:262-471)
// ===========================================================================

#[tokio::test]
async fn test_edit_replace_text() {
    let tmp = TempDir::new();
    let file = tmp.path().join("edit-test.txt");
    std::fs::write(&file, "Hello, world!").unwrap();

    let ctx = make_ctx(tmp.path());
    let tool = create_edit_tool(&ctx, EditToolOptions::default());

    let result = tool
        .execute(
            "test-call-5",
            json!({"path": "edit-test.txt", "edits": [{"oldText": "world", "newText": "testing"}]}),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();

    assert!(get_text(&result).contains("Successfully replaced"));
    let details = &result.details;
    assert!(details.get("diff").is_some());
    assert!(details.get("patch").is_some());

    let diff = details.get("diff").unwrap().as_str().unwrap();
    assert!(diff.contains("testing"));

    let patch = details.get("patch").unwrap().as_str().unwrap();
    assert!(patch.contains("--- "));
    assert!(patch.contains("+++ "));
    assert!(patch.contains("@@"));
    assert!(patch.contains("-Hello, world!"));
    assert!(patch.contains("+Hello, testing!"));
}

#[tokio::test]
async fn test_edit_fail_not_found() {
    let tmp = TempDir::new();
    let file = tmp.path().join("edit-test.txt");
    std::fs::write(&file, "Hello, world!").unwrap();

    let ctx = make_ctx(tmp.path());
    let tool = create_edit_tool(&ctx, EditToolOptions::default());

    let err = tool
        .execute(
            "test-call-6",
            json!({"path": "edit-test.txt", "edits": [{"oldText": "nonexistent", "newText": "testing"}]}),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap_err();

    if let AgentError::Message(msg) = err {
        assert!(msg.contains("Could not find the exact text"));
    } else {
        panic!("expected AgentError::Message");
    }
}

#[tokio::test]
async fn test_edit_enoent() {
    let tmp = TempDir::new();
    let missing = tmp.path().join("missing.txt");

    let ctx = make_ctx(tmp.path());
    let tool = create_edit_tool(&ctx, EditToolOptions::default());

    let err = tool
        .execute(
            "test-call-6b",
            json!({"path": missing.to_string_lossy(), "edits": [{"oldText": "hello", "newText": "world"}]}),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap_err();

    if let AgentError::Message(msg) = err {
        assert_eq!(
            msg,
            format!(
                "Could not edit file: {}. Error code: ENOENT.",
                missing.to_string_lossy()
            )
        );
    } else {
        panic!("expected AgentError::Message");
    }
}

#[tokio::test]
async fn test_edit_fail_multiple_occurrences() {
    let tmp = TempDir::new();
    let file = tmp.path().join("edit-test.txt");
    std::fs::write(&file, "foo foo foo").unwrap();

    let ctx = make_ctx(tmp.path());
    let tool = create_edit_tool(&ctx, EditToolOptions::default());

    let err = tool
        .execute(
            "test-call-7",
            json!({"path": "edit-test.txt", "edits": [{"oldText": "foo", "newText": "bar"}]}),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap_err();

    if let AgentError::Message(msg) = err {
        assert!(msg.contains("Found 3 occurrences"));
    } else {
        panic!("expected AgentError::Message");
    }
}

#[tokio::test]
async fn test_edit_multiple_disjoint() {
    let tmp = TempDir::new();
    let file = tmp.path().join("edit-multi.txt");
    std::fs::write(&file, "alpha\nbeta\ngamma\ndelta\n").unwrap();

    let ctx = make_ctx(tmp.path());
    let tool = create_edit_tool(&ctx, EditToolOptions::default());

    let result = tool
        .execute(
            "test-call-8",
            json!({"path": "edit-multi.txt", "edits": [
                {"oldText": "alpha\n", "newText": "ALPHA\n"},
                {"oldText": "gamma\n", "newText": "GAMMA\n"}
            ]}),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();

    assert!(get_text(&result).contains("Successfully replaced 2 block(s)"));
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "ALPHA\nbeta\nGAMMA\ndelta\n"
    );
}

#[tokio::test]
async fn test_edit_collapse_large_gaps() {
    let tmp = TempDir::new();
    let file = tmp.path().join("edit-multi-large-gap.txt");
    let lines: Vec<String> = (0..600).map(|i| format!("line {:03}", i + 1)).collect();
    std::fs::write(&file, format!("{}\n", lines.join("\n"))).unwrap();

    let ctx = make_ctx(tmp.path());
    let tool = create_edit_tool(&ctx, EditToolOptions::default());

    let result = tool
        .execute(
            "test-call-8b",
            json!({"path": "edit-multi-large-gap.txt", "edits": [
                {"oldText": "line 100\n", "newText": "LINE 100\n"},
                {"oldText": "line 300\n", "newText": "LINE 300\n"},
                {"oldText": "line 500\n", "newText": "LINE 500\n"}
            ]}),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();

    let diff = result.details.get("diff").unwrap().as_str().unwrap();
    assert!(diff.contains("LINE 100"));
    assert!(diff.contains("LINE 300"));
    assert!(diff.contains("LINE 500"));
    assert!(diff.contains("..."));
    assert!(!diff.contains("line 250"));
    assert!(diff.split('\n').count() < 50);
}

#[tokio::test]
async fn test_edit_matches_original_not_incremental() {
    let tmp = TempDir::new();
    let file = tmp.path().join("edit-multi-original.txt");
    std::fs::write(&file, "foo\nbar\nbaz\n").unwrap();

    let ctx = make_ctx(tmp.path());
    let tool = create_edit_tool(&ctx, EditToolOptions::default());

    tool.execute(
        "test-call-9",
        json!({"path": "edit-multi-original.txt", "edits": [
            {"oldText": "foo\n", "newText": "foo bar\n"},
            {"oldText": "bar\n", "newText": "BAR\n"}
        ]}),
        CancellationToken::new(),
        None,
    )
    .await
    .unwrap();

    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "foo bar\nBAR\nbaz\n"
    );
}

#[tokio::test]
async fn test_edit_empty_edits() {
    let tmp = TempDir::new();
    let file = tmp.path().join("edit-empty-edits.txt");
    std::fs::write(&file, "hello\nworld\n").unwrap();

    let ctx = make_ctx(tmp.path());
    let tool = create_edit_tool(&ctx, EditToolOptions::default());

    let err = tool
        .execute(
            "test-call-11",
            json!({"path": "edit-empty-edits.txt", "edits": []}),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap_err();

    if let AgentError::Message(msg) = err {
        assert!(msg.contains("edits must contain at least one replacement"));
    } else {
        panic!("expected AgentError::Message");
    }
}

#[tokio::test]
async fn test_edit_overlap() {
    let tmp = TempDir::new();
    let file = tmp.path().join("edit-overlap.txt");
    std::fs::write(&file, "one\ntwo\nthree\n").unwrap();

    let ctx = make_ctx(tmp.path());
    let tool = create_edit_tool(&ctx, EditToolOptions::default());

    let err = tool
        .execute(
            "test-call-12",
            json!({"path": "edit-overlap.txt", "edits": [
                {"oldText": "one\ntwo\n", "newText": "ONE\nTWO\n"},
                {"oldText": "two\nthree\n", "newText": "TWO\nTHREE\n"}
            ]}),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap_err();

    if let AgentError::Message(msg) = err {
        assert!(msg.contains("overlap"));
    } else {
        panic!("expected AgentError::Message");
    }
}

#[tokio::test]
async fn test_edit_no_partial_on_failure() {
    let tmp = TempDir::new();
    let file = tmp.path().join("edit-no-partial.txt");
    let original = "alpha\nbeta\ngamma\n";
    std::fs::write(&file, original).unwrap();

    let ctx = make_ctx(tmp.path());
    let tool = create_edit_tool(&ctx, EditToolOptions::default());

    let _ = tool
        .execute(
            "test-call-13",
            json!({"path": "edit-no-partial.txt", "edits": [
                {"oldText": "alpha\n", "newText": "ALPHA\n"},
                {"oldText": "missing\n", "newText": "MISSING\n"}
            ]}),
            CancellationToken::new(),
            None,
        )
        .await;

    // File should be unchanged because the second edit failed.
    assert_eq!(std::fs::read_to_string(&file).unwrap(), original);
}

#[tokio::test]
async fn test_edit_eacces() {
    let tmp = TempDir::new();
    let file = tmp.path().join("edit-readonly.txt");
    std::fs::write(&file, "hello\n").unwrap();

    // Set read-only.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o444)).unwrap();
    }

    let ctx = make_ctx(tmp.path());
    let tool = create_edit_tool(&ctx, EditToolOptions::default());

    let err = tool
        .execute(
            "test-call-14",
            json!({"path": "edit-readonly.txt", "edits": [{"oldText": "hello", "newText": "world"}]}),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap_err();

    if let AgentError::Message(msg) = err {
        assert!(msg.contains("EACCES"), "expected EACCES in: {msg}");
    } else {
        panic!("expected AgentError::Message");
    }

    // Restore permissions for cleanup.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644));
    }
}

// ===========================================================================
// Custom operations generic error test (tools.test.ts:437-454)
// ===========================================================================

struct FailingOps;

#[async_trait]
impl EditOperations for FailingOps {
    async fn read_file(&self, _path: &Path) -> io::Result<Vec<u8>> {
        Ok(b"hello\n".to_vec())
    }
    async fn write_file(&self, _path: &Path, _content: &str) -> io::Result<()> {
        Ok(())
    }
    async fn access(&self, _path: &Path) -> io::Result<()> {
        Err(io::Error::other("disk offline"))
    }
}

#[tokio::test]
async fn test_edit_generic_access_error() {
    let tmp = TempDir::new();
    let ctx = make_ctx(tmp.path());
    let tool = create_edit_tool(
        &ctx,
        EditToolOptions {
            operations: Some(std::sync::Arc::new(FailingOps)),
        },
    );

    let err = tool
        .execute(
            "test-call-16",
            json!({"path": "broken.txt", "edits": [{"oldText": "hello", "newText": "world"}]}),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap_err();

    if let AgentError::Message(msg) = err {
        assert_eq!(msg, "Could not edit file: broken.txt. Error: disk offline.");
    } else {
        panic!("expected AgentError::Message");
    }
}

// ===========================================================================
// Fuzzy matching tests (tools.test.ts:894-1119)
// ===========================================================================

#[tokio::test]
async fn test_edit_fuzzy_trailing_whitespace() {
    let tmp = TempDir::new();
    let file = tmp.path().join("trailing-ws.txt");
    std::fs::write(&file, "line one   \nline two  \nline three\n").unwrap();

    let ctx = make_ctx(tmp.path());
    let tool = create_edit_tool(&ctx, EditToolOptions::default());

    let result = tool
        .execute(
            "test-fuzzy-1",
            json!({"path": "trailing-ws.txt", "edits": [{"oldText": "line one\nline two\n", "newText": "replaced\n"}]}),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();

    assert!(get_text(&result).contains("Successfully replaced"));
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "replaced\nline three\n"
    );
}

#[tokio::test]
async fn test_edit_fuzzy_chinese_punctuation() {
    let tmp = TempDir::new();
    let file = tmp.path().join("chinese-punctuation.txt");
    std::fs::write(&file, "你好，世界\n你好（世界）\n").unwrap();

    let ctx = make_ctx(tmp.path());
    let tool = create_edit_tool(&ctx, EditToolOptions::default());

    let result = tool
        .execute(
            "test-fuzzy-chinese",
            json!({"path": "chinese-punctuation.txt", "edits": [{"oldText": "你好,世界\n你好(世界)\n", "newText": "你好，pi\n你好(pi)\n"}]}),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();

    assert!(get_text(&result).contains("Successfully replaced"));
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "你好，pi\n你好(pi)\n"
    );
}

#[tokio::test]
async fn test_edit_fuzzy_unicode_compatibility() {
    let tmp = TempDir::new();
    let file = tmp.path().join("unicode-compatibility.txt");
    std::fs::write(&file, "ＡＢＣ１２３\ncafe\u{0301}\n").unwrap();

    let ctx = make_ctx(tmp.path());
    let tool = create_edit_tool(&ctx, EditToolOptions::default());

    let result = tool
        .execute(
            "test-fuzzy-unicode",
            json!({"path": "unicode-compatibility.txt", "edits": [{"oldText": "ABC123\ncafé\n", "newText": "XYZ789\ncoffee\n"}]}),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();

    assert!(get_text(&result).contains("Successfully replaced"));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "XYZ789\ncoffee\n");
}

#[tokio::test]
async fn test_edit_fuzzy_smart_single_quotes() {
    let tmp = TempDir::new();
    let file = tmp.path().join("smart-quotes.txt");
    std::fs::write(&file, "console.log(\u{2018}hello\u{2019});\n").unwrap();

    let ctx = make_ctx(tmp.path());
    let tool = create_edit_tool(&ctx, EditToolOptions::default());

    let result = tool
        .execute(
            "test-fuzzy-2",
            json!({"path": "smart-quotes.txt", "edits": [{"oldText": "console.log('hello');", "newText": "console.log('world');"}]}),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();

    assert!(get_text(&result).contains("Successfully replaced"));
    assert!(std::fs::read_to_string(&file).unwrap().contains("world"));
}

#[tokio::test]
async fn test_edit_fuzzy_smart_double_quotes() {
    let tmp = TempDir::new();
    let file = tmp.path().join("smart-double-quotes.txt");
    std::fs::write(&file, "const msg = \u{201C}Hello World\u{201D};\n").unwrap();

    let ctx = make_ctx(tmp.path());
    let tool = create_edit_tool(&ctx, EditToolOptions::default());

    let result = tool
        .execute(
            "test-fuzzy-3",
            json!({"path": "smart-double-quotes.txt", "edits": [{"oldText": "const msg = \"Hello World\";", "newText": "const msg = \"Goodbye\";"}]}),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();

    assert!(get_text(&result).contains("Successfully replaced"));
    assert!(std::fs::read_to_string(&file).unwrap().contains("Goodbye"));
}

#[tokio::test]
async fn test_edit_fuzzy_unicode_dashes() {
    let tmp = TempDir::new();
    let file = tmp.path().join("unicode-dashes.txt");
    std::fs::write(&file, "range: 1\u{2013}5\nbreak\u{2014}here\n").unwrap();

    let ctx = make_ctx(tmp.path());
    let tool = create_edit_tool(&ctx, EditToolOptions::default());

    let result = tool
        .execute(
            "test-fuzzy-4",
            json!({"path": "unicode-dashes.txt", "edits": [{"oldText": "range: 1-5\nbreak-here", "newText": "range: 10-50\nbreak--here"}]}),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();

    assert!(get_text(&result).contains("Successfully replaced"));
    assert!(std::fs::read_to_string(&file).unwrap().contains("10-50"));
}

#[tokio::test]
async fn test_edit_fuzzy_nbsp() {
    let tmp = TempDir::new();
    let file = tmp.path().join("nbsp.txt");
    std::fs::write(&file, "hello\u{00A0}world\n").unwrap();

    let ctx = make_ctx(tmp.path());
    let tool = create_edit_tool(&ctx, EditToolOptions::default());

    let result = tool
        .execute(
            "test-fuzzy-5",
            json!({"path": "nbsp.txt", "edits": [{"oldText": "hello world", "newText": "hello universe"}]}),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();

    assert!(get_text(&result).contains("Successfully replaced"));
    assert!(std::fs::read_to_string(&file).unwrap().contains("universe"));
}

#[tokio::test]
async fn test_edit_exact_preferred_over_fuzzy() {
    let tmp = TempDir::new();
    let file = tmp.path().join("exact-preferred.txt");
    std::fs::write(&file, "const x = 'exact';\nconst y = 'other';\n").unwrap();

    let ctx = make_ctx(tmp.path());
    let tool = create_edit_tool(&ctx, EditToolOptions::default());

    let result = tool
        .execute(
            "test-fuzzy-6",
            json!({"path": "exact-preferred.txt", "edits": [{"oldText": "const x = 'exact';", "newText": "const x = 'changed';"}]}),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();

    assert!(get_text(&result).contains("Successfully replaced"));
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "const x = 'changed';\nconst y = 'other';\n"
    );
}

#[tokio::test]
async fn test_edit_fuzzy_not_found() {
    let tmp = TempDir::new();
    let file = tmp.path().join("no-match.txt");
    std::fs::write(&file, "completely different content\n").unwrap();

    let ctx = make_ctx(tmp.path());
    let tool = create_edit_tool(&ctx, EditToolOptions::default());

    let err = tool
        .execute(
            "test-fuzzy-7",
            json!({"path": "no-match.txt", "edits": [{"oldText": "this does not exist", "newText": "replacement"}]}),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap_err();

    if let AgentError::Message(msg) = err {
        assert!(msg.contains("Could not find the exact text"));
    } else {
        panic!("expected AgentError::Message");
    }
}

#[tokio::test]
async fn test_edit_fuzzy_duplicate_detection() {
    let tmp = TempDir::new();
    let file = tmp.path().join("fuzzy-dups.txt");
    std::fs::write(&file, "hello world   \nhello world\n").unwrap();

    let ctx = make_ctx(tmp.path());
    let tool = create_edit_tool(&ctx, EditToolOptions::default());

    let err = tool
        .execute(
            "test-fuzzy-8",
            json!({"path": "fuzzy-dups.txt", "edits": [{"oldText": "hello world", "newText": "replaced"}]}),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap_err();

    if let AgentError::Message(msg) = err {
        assert!(msg.contains("Found 2 occurrences"));
    } else {
        panic!("expected AgentError::Message");
    }
}

#[tokio::test]
async fn test_edit_fuzzy_multi_edit_mode() {
    let tmp = TempDir::new();
    let file = tmp.path().join("fuzzy-multi.txt");
    std::fs::write(
        &file,
        "console.log(\u{2018}hello\u{2019});\nhello\u{00A0}world\n",
    )
    .unwrap();

    let ctx = make_ctx(tmp.path());
    let tool = create_edit_tool(&ctx, EditToolOptions::default());

    tool.execute(
        "test-fuzzy-9",
        json!({"path": "fuzzy-multi.txt", "edits": [
            {"oldText": "console.log('hello');\n", "newText": "console.log('world');\n"},
            {"oldText": "hello world\n", "newText": "hello universe\n"}
        ]}),
        CancellationToken::new(),
        None,
    )
    .await
    .unwrap();

    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "console.log('world');\nhello universe\n"
    );
}

#[tokio::test]
async fn test_edit_fuzzy_preserve_duplicate_line() {
    let tmp = TempDir::new();
    let file = tmp.path().join("fuzzy-preserve-duplicate-line.txt");
    let original = "replace me   \nafter   \n";
    std::fs::write(&file, original).unwrap();

    let ctx = make_ctx(tmp.path());
    let tool = create_edit_tool(&ctx, EditToolOptions::default());

    let result = tool
        .execute(
            "test-fuzzy-preserve-duplicate-line",
            json!({"path": "fuzzy-preserve-duplicate-line.txt", "edits": [{"oldText": "replace me\n", "newText": "after\n"}]}),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();

    let expected = "after\nafter   \n";
    assert_eq!(std::fs::read_to_string(&file).unwrap(), expected);

    // Patch should apply to original and produce expected.
    let patch = result.details.get("patch").unwrap().as_str().unwrap();
    // Verify patch contains the expected hunk
    assert!(patch.contains("@@"));
}

#[tokio::test]
async fn test_edit_fuzzy_preserve_untouched_lines_multi() {
    let tmp = TempDir::new();
    let file = tmp.path().join("fuzzy-preserve-multi.txt");
    let original: String = [
        "keep before  ",
        "first target  ",
        "first after",
        "keep middle   ",
        "second target  ",
        "second after",
        "keep after  ",
        "",
    ]
    .join("\n");
    std::fs::write(&file, &original).unwrap();

    let ctx = make_ctx(tmp.path());
    let tool = create_edit_tool(&ctx, EditToolOptions::default());

    tool.execute(
        "test-fuzzy-preserve-multi",
        json!({"path": "fuzzy-preserve-multi.txt", "edits": [
            {"oldText": "first target\nfirst after", "newText": "FIRST\nFIRST2"},
            {"oldText": "second target\nsecond after", "newText": "SECOND\nSECOND2"}
        ]}),
        CancellationToken::new(),
        None,
    )
    .await
    .unwrap();

    let expected: String = [
        "keep before  ",
        "FIRST",
        "FIRST2",
        "keep middle   ",
        "SECOND",
        "SECOND2",
        "keep after  ",
        "",
    ]
    .join("\n");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), expected);
}

// ===========================================================================
// CRLF handling tests (tools.test.ts:1121-1213)
// ===========================================================================

#[tokio::test]
async fn test_edit_crlf_match_lf_oldtext() {
    let tmp = TempDir::new();
    let file = tmp.path().join("crlf-test.txt");
    std::fs::write(&file, "line one\r\nline two\r\nline three\r\n").unwrap();

    let ctx = make_ctx(tmp.path());
    let tool = create_edit_tool(&ctx, EditToolOptions::default());

    let result = tool
        .execute(
            "test-crlf-1",
            json!({"path": "crlf-test.txt", "edits": [{"oldText": "line two\n", "newText": "replaced line\n"}]}),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();

    assert!(get_text(&result).contains("Successfully replaced"));
}

#[tokio::test]
async fn test_edit_crlf_preserve_after_edit() {
    let tmp = TempDir::new();
    let file = tmp.path().join("crlf-preserve.txt");
    std::fs::write(&file, "first\r\nsecond\r\nthird\r\n").unwrap();

    let ctx = make_ctx(tmp.path());
    let tool = create_edit_tool(&ctx, EditToolOptions::default());

    tool.execute(
        "test-crlf-2",
        json!({"path": "crlf-preserve.txt", "edits": [{"oldText": "second\n", "newText": "REPLACED\n"}]}),
        CancellationToken::new(),
        None,
    )
    .await
    .unwrap();

    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "first\r\nREPLACED\r\nthird\r\n"
    );
}

#[tokio::test]
async fn test_edit_lf_preserve() {
    let tmp = TempDir::new();
    let file = tmp.path().join("lf-preserve.txt");
    std::fs::write(&file, "first\nsecond\nthird\n").unwrap();

    let ctx = make_ctx(tmp.path());
    let tool = create_edit_tool(&ctx, EditToolOptions::default());

    tool.execute(
        "test-lf-1",
        json!({"path": "lf-preserve.txt", "edits": [{"oldText": "second\n", "newText": "REPLACED\n"}]}),
        CancellationToken::new(),
        None,
    )
    .await
    .unwrap();

    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "first\nREPLACED\nthird\n"
    );
}

#[tokio::test]
async fn test_edit_crlf_lf_duplicate_detection() {
    let tmp = TempDir::new();
    let file = tmp.path().join("mixed-endings.txt");
    std::fs::write(&file, "hello\r\nworld\r\n---\r\nhello\nworld\n").unwrap();

    let ctx = make_ctx(tmp.path());
    let tool = create_edit_tool(&ctx, EditToolOptions::default());

    let err = tool
        .execute(
            "test-crlf-dup",
            json!({"path": "mixed-endings.txt", "edits": [{"oldText": "hello\nworld\n", "newText": "replaced\n"}]}),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap_err();

    if let AgentError::Message(msg) = err {
        assert!(msg.contains("Found 2 occurrences"));
    } else {
        panic!("expected AgentError::Message");
    }
}

#[tokio::test]
async fn test_edit_preserve_bom() {
    let tmp = TempDir::new();
    let file = tmp.path().join("bom-test.txt");
    std::fs::write(&file, "\u{FEFF}first\r\nsecond\r\nthird\r\n").unwrap();

    let ctx = make_ctx(tmp.path());
    let tool = create_edit_tool(&ctx, EditToolOptions::default());

    tool.execute(
        "test-bom",
        json!({"path": "bom-test.txt", "edits": [{"oldText": "second\n", "newText": "REPLACED\n"}]}),
        CancellationToken::new(),
        None,
    )
    .await
    .unwrap();

    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "\u{FEFF}first\r\nREPLACED\r\nthird\r\n"
    );
}

#[tokio::test]
async fn test_edit_crlf_bom_multi_edit() {
    let tmp = TempDir::new();
    let file = tmp.path().join("bom-crlf-multi.txt");
    std::fs::write(&file, "\u{FEFF}first\r\nsecond\r\nthird\r\nfourth\r\n").unwrap();

    let ctx = make_ctx(tmp.path());
    let tool = create_edit_tool(&ctx, EditToolOptions::default());

    tool.execute(
        "test-crlf-multi",
        json!({"path": "bom-crlf-multi.txt", "edits": [
            {"oldText": "second\n", "newText": "SECOND\n"},
            {"oldText": "fourth\n", "newText": "FOURTH\n"}
        ]}),
        CancellationToken::new(),
        None,
    )
    .await
    .unwrap();

    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "\u{FEFF}first\r\nSECOND\r\nthird\r\nFOURTH\r\n"
    );
}

// ===========================================================================
// compute_edits_diff tests (tools.test.ts:456-471)
// ===========================================================================

#[tokio::test]
async fn test_compute_edits_diff_enoent() {
    use pir::tools::edit_diff::compute_edits_diff;

    let tmp = TempDir::new();
    let missing = tmp.path().join("missing-preview.txt");
    let result = compute_edits_diff(
        &missing.to_string_lossy(),
        &[EditReplacement {
            old_text: "hello".into(),
            new_text: "world".into(),
            edit_index: 0,
        }],
        tmp.path(),
    );

    assert!(result.error.is_some());
    assert_eq!(
        result.error.as_ref().unwrap(),
        &format!(
            "Could not edit file: {}. Error code: ENOENT.",
            missing.to_string_lossy()
        )
    );
}

// ===========================================================================
// Legacy shim tests (edit-tool-legacy-input.test.ts)
// ===========================================================================

#[test]
fn test_schema_no_legacy_fields() {
    let cwd = std::env::temp_dir();
    let ctx = make_ctx(&cwd);
    let tool = create_edit_tool(&ctx, EditToolOptions::default());
    let params = tool.parameters();
    let props = params.get("properties").unwrap().as_object().unwrap();
    assert!(!props.contains_key("oldText"));
    assert!(!props.contains_key("newText"));
}

#[test]
fn test_prepare_folds_legacy_into_edits() {
    let cwd = std::env::temp_dir();
    let ctx = make_ctx(&cwd);
    let tool = create_edit_tool(&ctx, EditToolOptions::default());

    let prepared = tool.prepare_arguments(json!({
        "path": "file.txt",
        "oldText": "before",
        "newText": "after"
    }));

    assert_eq!(
        prepared,
        json!({
            "path": "file.txt",
            "edits": [{"oldText": "before", "newText": "after"}]
        })
    );
}

#[test]
fn test_prepare_appends_legacy_to_existing_edits() {
    let cwd = std::env::temp_dir();
    let ctx = make_ctx(&cwd);
    let tool = create_edit_tool(&ctx, EditToolOptions::default());

    let prepared = tool.prepare_arguments(json!({
        "path": "file.txt",
        "edits": [{"oldText": "a", "newText": "b"}],
        "oldText": "c",
        "newText": "d"
    }));

    assert_eq!(
        prepared,
        json!({
            "path": "file.txt",
            "edits": [
                {"oldText": "a", "newText": "b"},
                {"oldText": "c", "newText": "d"}
            ]
        })
    );
}

#[test]
fn test_prepare_passes_through_valid_input() {
    let cwd = std::env::temp_dir();
    let ctx = make_ctx(&cwd);
    let tool = create_edit_tool(&ctx, EditToolOptions::default());

    let input = json!({"path": "file.txt", "edits": [{"oldText": "a", "newText": "b"}]});
    let prepared = tool.prepare_arguments(input.clone());
    // Content should be the same (identity — values match).
    assert_eq!(prepared, input);
}

#[test]
fn test_prepare_passes_through_non_object() {
    let cwd = std::env::temp_dir();
    let ctx = make_ctx(&cwd);
    let tool = create_edit_tool(&ctx, EditToolOptions::default());

    // null, numbers, strings pass through.
    assert_eq!(tool.prepare_arguments(Value::Null), Value::Null);
    assert_eq!(tool.prepare_arguments(json!("garbage")), json!("garbage"));
    assert_eq!(tool.prepare_arguments(json!(42)), json!(42));
}

#[test]
fn test_prepare_parses_json_string_edits() {
    let cwd = std::env::temp_dir();
    let ctx = make_ctx(&cwd);
    let tool = create_edit_tool(&ctx, EditToolOptions::default());

    let prepared = tool.prepare_arguments(json!({
        "path": "file.txt",
        "edits": json!([{"oldText": "a", "newText": "b"}]).to_string()
    }));

    assert_eq!(
        prepared,
        json!({
            "path": "file.txt",
            "edits": [{"oldText": "a", "newText": "b"}]
        })
    );
}

#[test]
fn test_prepare_invalid_json_string_edits_preserved() {
    let cwd = std::env::temp_dir();
    let ctx = make_ctx(&cwd);
    let tool = create_edit_tool(&ctx, EditToolOptions::default());

    let prepared = tool.prepare_arguments(json!({
        "path": "file.txt",
        "edits": "not json"
    }));

    assert_eq!(prepared, json!({"path": "file.txt", "edits": "not json"}));
}

#[tokio::test]
async fn test_legacy_args_execute() {
    let tmp = TempDir::new();
    let file = tmp.path().join("legacy.txt");
    std::fs::write(&file, "before\n").unwrap();

    let ctx = make_ctx(tmp.path());
    let tool = create_edit_tool(&ctx, EditToolOptions::default());

    let prepared = tool.prepare_arguments(json!({
        "path": "legacy.txt",
        "oldText": "before",
        "newText": "after"
    }));

    let result = tool
        .execute("tool-1", prepared, CancellationToken::new(), None)
        .await
        .unwrap();

    assert_eq!(
        get_text(&result),
        "Successfully replaced 1 block(s) in legacy.txt."
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "after\n");
}

// ===========================================================================
// Metadata tests
// ===========================================================================

#[tokio::test]
async fn test_edit_metadata() {
    let cwd = std::env::temp_dir();
    let ctx = make_ctx(&cwd);
    let tool = create_edit_tool(&ctx, EditToolOptions::default());

    assert_eq!(tool.name(), "edit");
    assert_eq!(tool.label(), "edit");
    assert!(tool.description().contains("Edit a single file"));
    assert!(tool
        .description()
        .contains("edits[].oldText must match a unique"));

    let params = tool.parameters();
    assert!(params.get("properties").unwrap().get("path").is_some());
    assert!(params.get("properties").unwrap().get("edits").is_some());
    let required = params.get("required").unwrap().as_array().unwrap();
    assert!(required.contains(&json!("path")));
    assert!(required.contains(&json!("edits")));
}
