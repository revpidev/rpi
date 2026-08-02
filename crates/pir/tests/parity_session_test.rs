//! G3 对拍（T07）：`fixtures/generated/*/session.jsonl`（上游 coding-agent
//! 真实 session 实录）经 `pir::core::session_manager::SessionManager` 加载 →
//! 树/context 构建 → `export_jsonl()` 写回，与原始 fixture 归一化 diff。
//!
//! 覆盖两个层面：
//! - **无损往返**：打开 v3 fixture 后 export 的行序列与原文件在
//!   `pir_test_support` Normalizer 语义下一致（id/timestamp 一致映射）。
//! - **加载 + 续跑**：打开 fixture 后追加 user/assistant 消息，context 包含
//!   旧消息 + 新消息，文件追加两行，重开后状态完整。
//!
//! fixture 中没有 compaction/branch_summary/label 条目，这些形态由
//! `core::session_manager::tests` 的合成单测覆盖。

use std::path::PathBuf;

use pir::core::session_manager::SessionManager;
use pir_agent::messages::AgentMessage;
use pir_ai::types::{UserContent, UserMessage, UserRole};
use pir_test_support::diff::diff_jsonl;
use pir_test_support::faux::{faux_assistant_message, FauxAssistantOptions};
use serde_json::Value;

const SCENARIOS: &[&str] = &[
    "abort",
    "length-truncation",
    "single-turn",
    "steering-followup",
    "tool-calls",
];

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/generated")
}

struct TestDir(PathBuf);

impl TestDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "pir-parity-session-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("create parity session test dir");
        TestDir(dir)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn non_empty_lines(text: &str) -> Vec<&str> {
    text.lines().filter(|l| !l.trim().is_empty()).collect()
}

/// Copy the fixture session into a scratch dir (open derives the session dir
/// from the file's parent; the fixture dir itself stays read-only-by-convention).
fn stage_fixture(scenario: &str) -> (TestDir, PathBuf, String) {
    let fixture_path = fixtures_dir().join(scenario).join("session.jsonl");
    let original = std::fs::read_to_string(&fixture_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", fixture_path.display()));
    let dir = TestDir::new(scenario);
    let staged = dir.0.join("session.jsonl");
    std::fs::write(&staged, &original).expect("stage fixture copy");
    (dir, staged, original)
}

fn user_msg(text: &str) -> AgentMessage {
    AgentMessage::User(UserMessage {
        role: UserRole::User,
        content: UserContent::Text(text.to_owned()),
        timestamp: 1,
    })
}

#[test]
fn parity_fixture_sessions_load_and_export_lossless() {
    for scenario in SCENARIOS {
        let (_dir, staged, original) = stage_fixture(scenario);
        let lines = non_empty_lines(&original);
        let header: Value = serde_json::from_str(lines[0]).expect("fixture header parses");
        assert_eq!(
            header.get("type").and_then(Value::as_str),
            Some("session"),
            "{scenario}: first line is a session header"
        );
        let message_lines = lines
            .iter()
            .filter(|l| {
                serde_json::from_str::<Value>(l)
                    .ok()
                    .and_then(|v| v.get("type").and_then(Value::as_str).map(str::to_owned))
                    .as_deref()
                    == Some("message")
            })
            .count();

        let sm = SessionManager::open(&staged, None, None)
            .unwrap_or_else(|e| panic!("{scenario}: open: {e}"));

        // Header: v3, id preserved.
        assert_eq!(
            sm.get_session_id(),
            header["id"].as_str().expect("id"),
            "{scenario}"
        );

        // Every non-header line is an entry; tree is a single root (fixtures
        // are linear chains).
        let entries = sm.get_entries();
        assert_eq!(entries.len(), lines.len() - 1, "{scenario}: entry count");
        assert_eq!(sm.get_tree().len(), 1, "{scenario}: single-root tree");

        // Context contains exactly the message entries.
        let ctx = sm.build_session_context();
        assert_eq!(
            ctx.messages.len(),
            message_lines,
            "{scenario}: context messages"
        );

        // Lossless export: normalized diff against the original fixture.
        let exported = sm.export_jsonl().expect("export");
        let exported_header: Value =
            serde_json::from_str(non_empty_lines(&exported)[0]).expect("exported header parses");
        assert_eq!(
            exported_header["version"],
            Value::from(3),
            "{scenario}: header v3"
        );
        diff_jsonl(&original, &exported)
            .unwrap_or_else(|f| panic!("{scenario}: export parity diff:\n{f}"));
    }
}

#[test]
fn parity_fixture_sessions_continue_after_load() {
    for scenario in SCENARIOS {
        let (_dir, staged, original) = stage_fixture(scenario);
        let original_line_count = non_empty_lines(&original).len();

        let mut sm = SessionManager::open(&staged, None, None)
            .unwrap_or_else(|e| panic!("{scenario}: open: {e}"));
        let before_ctx = sm.build_session_context();
        let before_entries = sm.get_entries().len();

        sm.append_message(user_msg("continued question"))
            .expect("append user");
        sm.append_message(AgentMessage::Assistant(faux_assistant_message(
            "continued answer",
            FauxAssistantOptions::default(),
        )))
        .expect("append assistant");

        // Context keeps the old messages, then the new pair.
        let after_ctx = sm.build_session_context();
        assert_eq!(
            after_ctx.messages.len(),
            before_ctx.messages.len() + 2,
            "{scenario}: context grows by the appended pair"
        );
        assert_eq!(
            after_ctx.messages[..before_ctx.messages.len()],
            before_ctx.messages[..],
            "{scenario}: old context messages unchanged"
        );
        assert_eq!(
            sm.get_entries().len(),
            before_entries + 2,
            "{scenario}: entries"
        );

        // The file gained exactly two lines.
        let on_disk = std::fs::read_to_string(&staged).expect("read staged file");
        assert_eq!(
            non_empty_lines(&on_disk).len(),
            original_line_count + 2,
            "{scenario}: file appended"
        );

        // Reopen: full state survives a reload.
        let reopened = SessionManager::open(&staged, None, None)
            .unwrap_or_else(|e| panic!("{scenario}: reopen: {e}"));
        assert_eq!(
            reopened.get_entries().len(),
            before_entries + 2,
            "{scenario}: reload entries"
        );
        let reopened_ctx = reopened.build_session_context();
        assert_eq!(
            reopened_ctx.messages.len(),
            before_ctx.messages.len() + 2,
            "{scenario}: reload context"
        );
    }
}
