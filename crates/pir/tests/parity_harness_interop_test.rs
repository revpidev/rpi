//! G3 互通对拍（T16 自测清单「互通」条目）：harness JSONL 产物 ↔ T07 主路径
//! `SessionManager` 双向加载 + 既有 fixtures 交叉验证。
//!
//! 三个方向：
//! - **harness → 主路径**（`harness_session_loads_in_session_manager_*`）：
//!   用 `JsonlSessionStorage` + `Session` 门面构造会话（全部 11 种条目，
//!   `firstKeptEntryId` / `retainedTail` 两形态 compaction、`active_tools_change`、
//!   `leaf` 移动，含以 leaf 记录收尾的会话）写盘后由 `SessionManager` 打开：
//!   条目数/类型/载荷逐一保留、leaf 按 harness 语义重建、context 与 harness 侧
//!   一致（主路径读取时展开 retainedTail，见设计文档 §457 注记）、无损写回、
//!   续跑追加不丢数据。
//! - **主路径 → harness**（`session_manager_session_loads_in_harness_repo`）：
//!   `SessionManager` 构造等价会话写盘后经 `JsonlSessionRepo::open` 加载：
//!   header / 条目 / leaf 重建正确，`get_path_to_root_or_compaction` 两形态
//!   （无 compaction 全路径 + `firstKeptEntryId` 截断）行走与主路径一致，
//!   stats / name / label / context 一致。
//! - **fixtures 交叉**（`harness_storage_loads_all_fixture_sessions`）：
//!   `fixtures/generated/*/session.jsonl`（上游 coding-agent 实录；`compaction/`
//!   目录无 session.jsonl，compaction 场景在 `compaction-threshold` /
//!   `compaction-overflow`）逐一用 harness `JsonlSessionStorage` 加载成功
//!   （version 3 硬校验、条目全解析、leaf 正确），并与 `SessionManager` 三方
//!   交叉：typed 条目相等、leaf 相等、path-to-root-or-compaction 相等。
//!
//! harness 侧文件系统用 `NodeExecutionEnv`（真实 tokio 实现，非测试替身），
//! 因此本文件同时覆盖 storage 的真实 I/O 路径。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pir::core::session_manager::{NewSessionOptions, SessionManager, StoredEntry};
use pir_agent::harness::env::nodejs::NodeExecutionEnv;
use pir_agent::harness::session::jsonl_repo::JsonlSessionRepo;
use pir_agent::harness::session::jsonl_storage::{
    JsonlSessionStorage, JsonlSessionStorageCreateOptions,
};
use pir_agent::harness::session::Session as SessionFacade;
use pir_agent::harness::types::{
    AppendCompactionOptions, MoveToSummary, SessionContextBuildOptions,
};
use pir_agent::harness::{
    FileSystem, JsonlSessionMetadata, Session as SessionTrait, SessionEntryCursorOptions,
    SessionMetadata, SessionRepo, SessionStats, SessionStorage,
};
use pir_agent::messages::AgentMessage;
use pir_agent::session::{MessageEntry, SessionEntry};
use pir_ai::types::{
    AssistantContent, Usage, UsageCost, UserContent, UserContentBlock, UserMessage, UserRole,
};
use pir_test_support::faux::{faux_assistant_message, FauxAssistantOptions};
use serde_json::{json, Value};

const SCENARIOS: &[&str] = &[
    "abort",
    "compaction-threshold",
    "compaction-overflow",
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
            "pir-parity-harness-interop-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("create interop test dir");
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

fn user_msg(text: &str) -> AgentMessage {
    AgentMessage::User(UserMessage {
        role: UserRole::User,
        content: UserContent::Text(text.to_owned()),
        timestamp: 1,
    })
}

fn assistant_msg(text: &str) -> AgentMessage {
    AgentMessage::Assistant(faux_assistant_message(
        text,
        FauxAssistantOptions::default(),
    ))
}

fn assistant_msg_with_usage(text: &str, usage: Usage) -> AgentMessage {
    let mut message = faux_assistant_message(text, FauxAssistantOptions::default());
    message.usage = usage;
    AgentMessage::Assistant(message)
}

/// `Usage` factory — only `cost.total` is read by `getSessionStats`
/// (jsonl-storage.ts:326-335), so the cost components are zeroed.
fn usage(
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    total_tokens: u64,
    cost_total: f64,
) -> Usage {
    Usage {
        input,
        output,
        cache_read,
        cache_write,
        cache_write1h: None,
        reasoning: None,
        total_tokens,
        cost: UsageCost {
            input: 0.0,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
            total: cost_total,
        },
    }
}

/// `message.role` literal (upstream `message.role` comparisons).
fn role_tag(message: &AgentMessage) -> &'static str {
    match message {
        AgentMessage::User(_) => "user",
        AgentMessage::Assistant(_) => "assistant",
        AgentMessage::ToolResult(_) => "toolResult",
        AgentMessage::BashExecution(_) => "bashExecution",
        AgentMessage::Custom(_) => "custom",
        AgentMessage::BranchSummary(_) => "branchSummary",
        AgentMessage::CompactionSummary(_) => "compactionSummary",
    }
}

fn roles(messages: &[AgentMessage]) -> Vec<&'static str> {
    messages.iter().map(role_tag).collect()
}

fn text_of(message: &AgentMessage) -> String {
    match message {
        AgentMessage::User(user) => match &user.content {
            UserContent::Text(text) => text.clone(),
            UserContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|block| match block {
                    UserContentBlock::Text(text) => Some(text.text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" "),
        },
        AgentMessage::Assistant(assistant) => assistant
            .content
            .iter()
            .filter_map(|block| match block {
                AssistantContent::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" "),
        AgentMessage::Custom(custom) => match &custom.content {
            UserContent::Text(text) => text.clone(),
            UserContent::Blocks(_) => String::new(),
        },
        AgentMessage::CompactionSummary(summary) => summary.summary.clone(),
        AgentMessage::BranchSummary(summary) => summary.summary.clone(),
        _ => String::new(),
    }
}

fn texts(messages: &[AgentMessage]) -> Vec<String> {
    messages.iter().map(text_of).collect()
}

fn entry_ids(entries: &[SessionEntry]) -> Vec<&str> {
    entries.iter().map(SessionEntry::id).collect()
}

fn stored_ids(entries: &[StoredEntry]) -> Vec<&str> {
    entries.iter().map(|entry| entry.id()).collect()
}

/// Mirror of `JsonlSessionStorage::get_session_stats` (jsonl-storage.ts:308-348)
/// computed over typed entries — the cross-check for the main path's output.
fn expected_stats(entries: &[SessionEntry]) -> SessionStats {
    let mut stats = SessionStats::default();
    for entry in entries {
        if matches!(entry, SessionEntry::Message(_)) {
            stats.message_count = stats.message_count.saturating_add(1);
        }
        let usage = match entry {
            SessionEntry::Message(MessageEntry {
                message: AgentMessage::Assistant(assistant),
                ..
            }) => Some(&assistant.usage),
            SessionEntry::Compaction(compaction) => compaction.usage.as_ref(),
            SessionEntry::BranchSummary(branch_summary) => branch_summary.usage.as_ref(),
            _ => None,
        };
        if let Some(usage) = usage {
            stats.cached_tokens = stats.cached_tokens.saturating_add(usage.cache_read);
            stats.uncached_tokens = stats
                .uncached_tokens
                .saturating_add(usage.input.saturating_add(usage.cache_write));
            stats.total_tokens = stats.total_tokens.saturating_add(
                usage
                    .input
                    .saturating_add(usage.output)
                    .saturating_add(usage.cache_read)
                    .saturating_add(usage.cache_write),
            );
            stats.cost_total += usage.cost.total;
        }
    }
    stats
}

/// Build a `JsonlSessionStorage`-backed `Session` facade under `root`.
async fn harness_session(
    root: &Path,
    session_id: &str,
) -> (
    Arc<dyn FileSystem>,
    PathBuf,
    Arc<dyn SessionStorage<Metadata = JsonlSessionMetadata>>,
    Arc<dyn SessionTrait<Metadata = JsonlSessionMetadata>>,
) {
    let cwd = root.to_string_lossy().into_owned();
    let file_path = root.join("session.jsonl");
    let fs: Arc<dyn FileSystem> = Arc::new(NodeExecutionEnv::new(cwd.clone()));
    let storage: Arc<dyn SessionStorage<Metadata = JsonlSessionMetadata>> = Arc::new(
        JsonlSessionStorage::create(
            Arc::clone(&fs),
            &file_path.to_string_lossy(),
            JsonlSessionStorageCreateOptions {
                cwd,
                session_id: session_id.to_owned(),
                parent_session_path: None,
                metadata: None,
            },
        )
        .await
        .expect("create harness storage"),
    );
    let session: Arc<dyn SessionTrait<Metadata = JsonlSessionMetadata>> = Arc::new(
        SessionFacade::new(Arc::clone(&storage), SessionContextBuildOptions::default()),
    );
    (fs, file_path, storage, session)
}

// ===========================================================================
// 方向 1：harness → 主路径
// ===========================================================================

/// harness 会话（全部条目类型 + `firstKeptEntryId` 形态 compaction + 以
/// `leaf` 记录收尾）→ `SessionManager`：条目/载荷逐一保留、leaf 按 harness
/// 语义重建、context/name/label 一致、export 字节级无损、续跑后两加载器仍一致。
#[tokio::test]
async fn harness_session_loads_in_session_manager_preserving_all_entry_types() {
    let dir = TestDir::new("harness-to-main");
    let (fs, file_path, storage, session) = harness_session(&dir.0, "harness-session-a").await;

    // 1. 线性链 + 全部条目类型（含 harness 独有 active_tools_change / label /
    //    session_info / custom / custom_message），firstKeptEntryId 形态 compaction。
    let u1 = session.append_message(user_msg("one")).await.expect("u1");
    session
        .append_model_change("openai", "gpt-4.1")
        .await
        .expect("model change");
    session
        .append_thinking_level_change("high")
        .await
        .expect("thinking level change");
    session
        .append_active_tools_change(&["read".to_owned(), "bash".to_owned()])
        .await
        .expect("active tools change");
    session
        .append_message(assistant_msg("two"))
        .await
        .expect("a1");
    let u2 = session.append_message(user_msg("three")).await.expect("u2");
    let label_id = session
        .append_label(&u2, Some("bookmark"))
        .await
        .expect("label");
    let info_id = session
        .append_session_name("my session")
        .await
        .expect("session name");
    let custom_id = session
        .append_custom_entry("artifact-index", Some(json!({"n": 1})))
        .await
        .expect("custom");
    let cm_id = session
        .append_custom_message_entry(
            "note",
            UserContent::Text("note text".to_owned()),
            true,
            Some(json!({"ok": true})),
        )
        .await
        .expect("custom message");
    let compaction_id = session
        .append_compaction(
            "first-kept summary",
            Some(&u2),
            100,
            AppendCompactionOptions {
                usage: Some(usage(10, 5, 3, 2, 20, 1.5)),
                from_hook: Some(false),
                ..Default::default()
            },
        )
        .await
        .expect("compaction");
    let u3 = session.append_message(user_msg("four")).await.expect("u3");
    let a2 = session
        .append_message(assistant_msg("five"))
        .await
        .expect("a2");

    // Context（leaf = a2）：harness 的 buildContext 走 getBranch →
    // getPathToRootOrCompaction，路径在 firstKeptEntryId（u2）处截断。最后一个
    // compaction 生效，从 u2 起保留；label/session_info/custom 不产消息；
    // custom_message 投影为 custom。
    // 注：thinking_level / active_tool_names 从截断后的路径推导（上游继承差异：
    // harness session.ts 从分支路径推导，主路径 session-manager.ts 从全路径推导；
    // 此处 tlc/atc 在截断前，故为 off/None —— 两实现各自忠实于上游钉死行为，
    // 消息序列本身两实现一致，见下方主路径断言）。
    let mid_ctx = session
        .build_context(SessionContextBuildOptions::default())
        .await
        .expect("mid context");
    assert_eq!(
        roles(&mid_ctx.messages),
        ["compactionSummary", "user", "custom", "user", "assistant"]
    );
    assert_eq!(
        texts(&mid_ctx.messages),
        ["first-kept summary", "three", "note text", "four", "five"]
    );
    assert_eq!(mid_ctx.thinking_level, "off");
    assert_eq!(mid_ctx.active_tool_names, None);

    // 行走：从 a2 的 path-to-root-or-compaction 在 firstKeptEntryId（u2，含）处截断。
    let walk_a2 = storage
        .get_path_to_root_or_compaction(Some(&a2))
        .await
        .expect("walk a2");
    assert_eq!(
        entry_ids(&walk_a2),
        [
            u2.as_str(),
            label_id.as_str(),
            info_id.as_str(),
            custom_id.as_str(),
            cm_id.as_str(),
            compaction_id.as_str(),
            u3.as_str(),
            a2.as_str(),
        ]
    );

    // 2. 收尾操作是 moveTo —— 文件最后一条记录是 `leaf` 条目，harness leaf 为 u1。
    session.move_to(Some(&u1), None).await.expect("move to u1");
    assert_eq!(
        session.get_leaf_id().await.expect("leaf").as_deref(),
        Some(u1.as_str())
    );

    // 记录 harness 侧参照状态。
    let harness_entries = session
        .get_entries(SessionEntryCursorOptions::default())
        .await
        .expect("entries");
    let harness_ctx = session
        .build_context(SessionContextBuildOptions::default())
        .await
        .expect("context");
    assert_eq!(roles(&harness_ctx.messages), ["user"]);
    let harness_stats = storage.get_session_stats().await.expect("stats");
    let harness_name = storage.get_session_name().await.expect("name");
    let harness_label = storage.get_label(&u2).await.expect("label");
    let harness_walk = storage
        .get_path_to_root_or_compaction(Some(&u1))
        .await
        .expect("walk u1");
    assert_eq!(entry_ids(&harness_walk), [u1.as_str()]);
    drop(session);

    // 3. 主路径打开：条目数/类型/载荷逐一保留；leaf 与 harness 一致（leaf 重放）。
    let mut sm = SessionManager::open(&file_path, None, None).expect("main path open");
    assert_eq!(sm.get_session_id(), "harness-session-a");
    let sm_entries = sm.get_entries();
    assert_eq!(sm_entries.len(), harness_entries.len());
    for (expected, actual) in harness_entries.iter().zip(&sm_entries) {
        assert_eq!(expected, actual.known().expect("typed entry"));
    }
    assert_eq!(
        sm.get_leaf_id(),
        Some(u1.as_str()),
        "leaf rebuild must follow the harness leaf semantics"
    );

    // Context 与 harness 侧一致（messages / thinking_level / model；harness 另有
    // active_tool_names，主路径 context 无此字段）。
    let sm_ctx = sm.build_session_context();
    assert_eq!(sm_ctx.messages, harness_ctx.messages);
    assert_eq!(sm_ctx.thinking_level, harness_ctx.thinking_level);
    assert_eq!(
        sm_ctx
            .model
            .as_ref()
            .map(|m| (m.provider.as_str(), m.model_id.as_str())),
        harness_ctx
            .model
            .as_ref()
            .map(|m| (m.provider.as_str(), m.model_id.as_str())),
    );
    assert_eq!(sm.get_session_name(), harness_name);
    assert_eq!(sm.get_label(&u2), harness_label.as_deref());
    let sm_walk = sm
        .get_path_to_root_or_compaction(Some(&u1))
        .expect("main walk u1");
    assert_eq!(stored_ids(&sm_walk), [u1.as_str()]);

    // Stats：harness 加载器输出 == 对同一条目集的手工重算（主路径无 stats API）。
    assert_eq!(harness_stats, expected_stats(&harness_entries));

    // 4. 无损写回：version 3 无迁移，export 与 harness 文件字节级一致。
    let exported = sm.export_jsonl().expect("export");
    let original = std::fs::read_to_string(&file_path).expect("read harness file");
    assert_eq!(exported, original);

    // 主路径的设置推导走全路径（上游 session-manager.ts 语义）：同一 a2 叶，
    // thinking_level 为 "high"（tlc 在 firstKeptEntryId 截断之前，harness 侧
    // 为 "off"）——上游两实现本就如此，属继承差异而非互通缺陷。
    sm.branch(&a2).expect("branch to a2");
    assert_eq!(sm.build_session_context().thinking_level, "high");
    sm.branch(&u1).expect("branch back to u1");
    assert_eq!(sm.get_leaf_id(), Some(u1.as_str()));

    // 5. 续跑：追加不报错、文件 +2 行、重开 harness 后两加载器仍一致。
    sm.append_message(user_msg("continued question"))
        .expect("append user");
    sm.append_message(assistant_msg("continued answer"))
        .expect("append assistant");
    let after = std::fs::read_to_string(&file_path).expect("read after");
    assert_eq!(
        non_empty_lines(&after).len(),
        non_empty_lines(&original).len() + 2
    );
    let reopened: Arc<dyn SessionStorage<Metadata = JsonlSessionMetadata>> = Arc::new(
        JsonlSessionStorage::open(Arc::clone(&fs), &file_path.to_string_lossy())
            .await
            .expect("reopen harness storage"),
    );
    let reopened_facade: Arc<dyn SessionTrait<Metadata = JsonlSessionMetadata>> = Arc::new(
        SessionFacade::new(reopened, SessionContextBuildOptions::default()),
    );
    let reopened_entries = reopened_facade
        .get_entries(SessionEntryCursorOptions::default())
        .await
        .expect("entries");
    assert_eq!(reopened_entries.len(), harness_entries.len() + 2);
    let sm_ctx_after = sm.build_session_context();
    let reopened_ctx = reopened_facade
        .build_context(SessionContextBuildOptions::default())
        .await
        .expect("context");
    assert_eq!(reopened_ctx.messages, sm_ctx_after.messages);
    assert_eq!(
        reopened_ctx.messages[..harness_ctx.messages.len()],
        harness_ctx.messages[..]
    );
    assert_eq!(roles(&sm_ctx_after.messages), ["user", "user", "assistant"]);
}

/// harness 会话（`retainedTail` 形态 compaction + branch_summary + moveTo(root)
/// + 收尾 leaf 移动）→ `SessionManager`：retainedTail 在 context 中展开、两种
/// 行走形态两实现一致、分支/根重定向后的 leaf 与 context 一致。
#[tokio::test]
async fn harness_retained_tail_session_loads_in_session_manager() {
    let dir = TestDir::new("harness-retained-tail");
    let (_fs, file_path, storage, session) = harness_session(&dir.0, "harness-session-b").await;

    let u1 = session.append_message(user_msg("one")).await.expect("u1");
    session
        .append_message(assistant_msg("two"))
        .await
        .expect("a1");
    session.append_message(user_msg("three")).await.expect("u2");
    session
        .append_message(assistant_msg("four"))
        .await
        .expect("a2");
    let compaction_id = session
        .append_compaction(
            "retained summary",
            None,
            50,
            AppendCompactionOptions {
                retained_tail: Some(vec![user_msg("three"), assistant_msg("four")]),
                ..Default::default()
            },
        )
        .await
        .expect("compaction");
    let u3 = session.append_message(user_msg("five")).await.expect("u3");

    // retainedTail 形态：context = compactionSummary + 展开的 tail + 之后条目。
    let ctx_after_compaction = session
        .build_context(SessionContextBuildOptions::default())
        .await
        .expect("context");
    assert_eq!(
        roles(&ctx_after_compaction.messages),
        ["compactionSummary", "user", "assistant", "user"]
    );
    assert_eq!(
        texts(&ctx_after_compaction.messages),
        ["retained summary", "three", "four", "five"]
    );

    // 行走：自包含 compaction（retainedTail 形态）即路径终点 —— 从 u3 走
    // 只含 [compaction, u3]，之前条目不进入。
    let walk = storage
        .get_path_to_root_or_compaction(Some(&u3))
        .await
        .expect("walk u3");
    assert_eq!(entry_ids(&walk), [compaction_id.as_str(), u3.as_str()]);

    // 分支：moveTo + summary 在 u1 下追加 branch_summary，后续消息挂其下。
    let bs_id = session
        .move_to(
            Some(&u1),
            Some(MoveToSummary {
                summary: "branch text".to_owned(),
                ..Default::default()
            }),
        )
        .await
        .expect("move")
        .expect("summary id");
    let u4 = session.append_message(user_msg("six")).await.expect("u4");
    let ctx_branched = session
        .build_context(SessionContextBuildOptions::default())
        .await
        .expect("context");
    assert_eq!(
        roles(&ctx_branched.messages),
        ["user", "branchSummary", "user"]
    );
    let walk_u4 = storage
        .get_path_to_root_or_compaction(Some(&u4))
        .await
        .expect("walk u4");
    assert_eq!(
        entry_ids(&walk_u4),
        [u1.as_str(), bs_id.as_str(), u4.as_str()]
    );

    // 移到根：leaf 为 None、context 清空、后续追加挂在 null 下。
    session.move_to(None, None).await.expect("move to root");
    assert_eq!(session.get_leaf_id().await.expect("leaf"), None);
    assert!(session
        .build_context(SessionContextBuildOptions::default())
        .await
        .expect("context")
        .messages
        .is_empty());
    let u5 = session.append_message(user_msg("seven")).await.expect("u5");
    let u5_entry = session.get_entry(&u5).await.expect("entry").expect("found");
    assert_eq!(u5_entry.parent_id(), None);

    // 收尾 moveTo(u1)：文件最后一条记录是 leaf 条目。
    session.move_to(Some(&u1), None).await.expect("final move");

    let harness_entries = session
        .get_entries(SessionEntryCursorOptions::default())
        .await
        .expect("entries");
    let harness_ctx = session
        .build_context(SessionContextBuildOptions::default())
        .await
        .expect("context");
    assert_eq!(roles(&harness_ctx.messages), ["user"]);
    drop(session);

    // 主路径打开：条目/leaf 一致，两形态行走一致，context 一致。
    let sm = SessionManager::open(&file_path, None, None).expect("main path open");
    let sm_entries = sm.get_entries();
    assert_eq!(sm_entries.len(), harness_entries.len());
    for (expected, actual) in harness_entries.iter().zip(&sm_entries) {
        assert_eq!(expected, actual.known().expect("typed entry"));
    }
    assert_eq!(sm.get_leaf_id(), Some(u1.as_str()));

    let sm_walk = sm
        .get_path_to_root_or_compaction(Some(&u3))
        .expect("walk u3");
    assert_eq!(stored_ids(&sm_walk), entry_ids(&walk));
    let sm_walk_u4 = sm
        .get_path_to_root_or_compaction(Some(&u4))
        .expect("walk u4");
    assert_eq!(stored_ids(&sm_walk_u4), entry_ids(&walk_u4));

    let sm_ctx = sm.build_session_context();
    assert_eq!(sm_ctx.messages, harness_ctx.messages);
    assert_eq!(sm_ctx.thinking_level, harness_ctx.thinking_level);
    assert_eq!(sm_ctx.model, None);
    assert_eq!(harness_ctx.model, None);
}

// ===========================================================================
// 方向 2：主路径 → harness
// ===========================================================================

/// `SessionManager` 构造的会话（`firstKeptEntryId` 形态 compaction、branch、
/// label/session_info/custom/custom_message、usage）写盘后经
/// `JsonlSessionRepo::open` 加载：header/entries/leaf 重建正确、
/// `get_path_to_root_or_compaction` 两形态行走与主路径一致、stats/name/label/
/// context 一致。
#[tokio::test]
async fn session_manager_session_loads_in_harness_repo() {
    let dir = TestDir::new("main-to-harness");
    let cwd = dir.0.to_string_lossy().into_owned();
    let session_dir = dir.0.join("sessions");

    let mut sm = SessionManager::create(
        Path::new(&cwd),
        Some(&session_dir),
        NewSessionOptions {
            id: Some("main-session-c".to_owned()),
            parent_session: None,
        },
    )
    .expect("create");
    let u1 = sm.append_message(user_msg("one")).expect("u1");
    sm.append_message(assistant_msg_with_usage(
        "two",
        usage(10, 20, 30, 40, 100, 2.0),
    ))
    .expect("a1");
    sm.append_model_change("openai", "gpt-4.1")
        .expect("model change");
    sm.append_thinking_level_change("medium")
        .expect("thinking level change");
    let u2 = sm.append_message(user_msg("three")).expect("u2");
    sm.append_label_change(&u2, Some("bookmark"))
        .expect("label");
    sm.append_session_info("sm session").expect("session info");
    sm.append_custom_entry("artifact-index", Some(json!({"n": 1})))
        .expect("custom");
    sm.append_custom_message_entry(
        "note",
        UserContent::Text("note text".to_owned()),
        true,
        Some(json!({"ok": true})),
    )
    .expect("custom message");
    sm.append_compaction(
        "first-kept summary",
        &u2,
        200,
        Some(json!({"readFiles": []})),
        Some(false),
        Some(usage(5, 5, 5, 5, 20, 1.0)),
    )
    .expect("compaction");
    sm.append_message(user_msg("four")).expect("u3");
    let a2 = sm
        .append_message(assistant_msg_with_usage("five", usage(1, 2, 3, 4, 10, 0.5)))
        .expect("a2");
    sm.branch_with_summary(
        Some(&u1),
        "branch text",
        None,
        Some(false),
        Some(usage(2, 2, 2, 2, 8, 0.25)),
    )
    .expect("branch summary");
    let u4 = sm.append_message(user_msg("six")).expect("u4");

    let file = sm.get_session_file().expect("session file").to_path_buf();
    assert!(file.exists(), "session file persisted");
    let header = sm.get_header().expect("header").clone();
    let sm_entries = sm.get_entries();
    let sm_leaf = sm.get_leaf_id().expect("leaf").to_owned();

    // 用 header + 文件路径构造 harness 侧 metadata 并打开（repo 的目录布局
    // 是 `<root>/--<cwd>--/...`，主路径直接平铺在 session_dir 下，故不走 list）。
    let fs: Arc<dyn FileSystem> = Arc::new(NodeExecutionEnv::new(cwd.clone()));
    let repo = JsonlSessionRepo::new(fs, session_dir.to_string_lossy().into_owned());
    let metadata = JsonlSessionMetadata {
        base: SessionMetadata {
            id: header.id.clone(),
            created_at: header.timestamp.clone(),
        },
        cwd: header.cwd.clone(),
        path: file.to_string_lossy().into_owned(),
        parent_session_path: header.parent_session.clone(),
        metadata: None,
    };
    let opened = repo
        .open(metadata.clone())
        .await
        .expect("harness repo open");
    assert_eq!(
        opened.get_metadata().await.expect("metadata"),
        metadata,
        "header round-trips through the harness loader"
    );

    // 条目解析为完全相同的 typed 值；leaf 重建为最后一条记录。
    let storage = opened.storage();
    let entries = storage
        .get_entries(SessionEntryCursorOptions::default())
        .await
        .expect("entries");
    assert_eq!(entries.len(), sm_entries.len());
    for (expected, actual) in sm_entries.iter().zip(&entries) {
        assert_eq!(expected.known().expect("typed"), actual);
    }
    assert_eq!(
        storage.get_leaf_id().await.expect("leaf").as_deref(),
        Some(sm_leaf.as_str())
    );

    // get_path_to_root_or_compaction 两形态：分支路径（无 compaction，全路径）
    // 与从 a2 的 firstKeptEntryId 截断。
    let harness_walk = storage
        .get_path_to_root_or_compaction(Some(&u4))
        .await
        .expect("walk u4");
    let main_walk = sm
        .get_path_to_root_or_compaction(Some(&u4))
        .expect("main walk u4");
    assert_eq!(entry_ids(&harness_walk), stored_ids(&main_walk));
    let harness_walk_cut = storage
        .get_path_to_root_or_compaction(Some(&a2))
        .await
        .expect("walk a2");
    let main_walk_cut = sm
        .get_path_to_root_or_compaction(Some(&a2))
        .expect("main walk a2");
    assert_eq!(entry_ids(&harness_walk_cut), stored_ids(&main_walk_cut));

    // Stats：harness 加载器输出 == 对 typed 条目的手工重算。
    assert_eq!(
        storage.get_session_stats().await.expect("stats"),
        expected_stats(&entries)
    );

    // name / label / context 一致。
    assert_eq!(
        storage.get_session_name().await.expect("name"),
        sm.get_session_name()
    );
    assert_eq!(
        storage.get_label(&u2).await.expect("label").as_deref(),
        sm.get_label(&u2)
    );
    let harness_ctx = opened
        .build_context(SessionContextBuildOptions::default())
        .await
        .expect("context");
    let main_ctx = sm.build_session_context();
    assert_eq!(harness_ctx.messages, main_ctx.messages);
    assert_eq!(harness_ctx.thinking_level, main_ctx.thinking_level);
    assert_eq!(
        harness_ctx
            .model
            .as_ref()
            .map(|m| (m.provider.as_str(), m.model_id.as_str())),
        main_ctx
            .model
            .as_ref()
            .map(|m| (m.provider.as_str(), m.model_id.as_str()))
    );
    assert_eq!(harness_ctx.active_tool_names, None);
}

// ===========================================================================
// 方向 3：fixtures 交叉验证
// ===========================================================================

/// `fixtures/generated/*/session.jsonl`（上游 coding-agent 实录）逐一被
/// harness `JsonlSessionStorage` 加载：version 3 硬校验通过、条目全解析、
/// leaf 正确、path-to-root-or-compaction 行走完成，并与 T07 `SessionManager`
/// 三方交叉（typed 条目 / leaf / 行走均相等）。
#[tokio::test]
async fn harness_storage_loads_all_fixture_sessions() {
    for scenario in SCENARIOS {
        let fixture_path = fixtures_dir().join(scenario).join("session.jsonl");
        let original = std::fs::read_to_string(&fixture_path)
            .unwrap_or_else(|e| panic!("{scenario}: read fixture: {e}"));
        let lines: Vec<&str> = non_empty_lines(&original);
        assert!(lines.len() > 1, "{scenario}: session has entries");

        let dir = TestDir::new(&format!("fixture-{scenario}"));
        let staged = dir.0.join("session.jsonl");
        std::fs::write(&staged, &original).expect("stage fixture copy");
        let fs: Arc<dyn FileSystem> =
            Arc::new(NodeExecutionEnv::new(dir.0.to_string_lossy().into_owned()));

        // Version 3 硬校验 + 全量解析：每一行都加载为 typed 条目。
        let storage = JsonlSessionStorage::open(Arc::clone(&fs), &staged.to_string_lossy())
            .await
            .unwrap_or_else(|e| panic!("{scenario}: harness open: {}", e.message));
        let entries = storage
            .get_entries(SessionEntryCursorOptions::default())
            .await
            .expect("entries");
        assert_eq!(entries.len(), lines.len() - 1, "{scenario}: entry count");

        // Header 元数据来自首行；leaf 重建为最后一条记录（fixtures 是线性链）。
        let metadata = storage.get_metadata().await.expect("metadata");
        let header: Value = serde_json::from_str(lines[0]).expect("header json");
        assert_eq!(
            metadata.base.id,
            header["id"].as_str().expect("header id"),
            "{scenario}"
        );
        let leaf = storage.get_leaf_id().await.expect("leaf");
        assert_eq!(
            leaf.as_deref(),
            Some(entries.last().expect("last entry").id()),
            "{scenario}: leaf"
        );

        // 行走从 leaf 完成（有 compaction 的场景在截断处停，否则到根）。
        let walk = storage
            .get_path_to_root_or_compaction(leaf.as_deref())
            .await
            .expect("walk");
        assert_eq!(
            walk.last().expect("walk leaf").id(),
            entries.last().expect("last entry").id(),
            "{scenario}: walk ends at the leaf"
        );

        // 与 T07 主路径交叉：typed 条目 / leaf / 行走相等。
        let sm = SessionManager::open(&staged, None, None)
            .unwrap_or_else(|e| panic!("{scenario}: main path open: {e}"));
        let sm_entries = sm.get_entries();
        assert_eq!(
            sm_entries.len(),
            entries.len(),
            "{scenario}: entry count parity"
        );
        for (expected, actual) in entries.iter().zip(&sm_entries) {
            assert_eq!(
                expected,
                actual.known().expect("typed entry"),
                "{scenario}: entry parity"
            );
        }
        assert_eq!(sm.get_leaf_id(), leaf.as_deref(), "{scenario}: leaf parity");
        let sm_walk = sm
            .get_path_to_root_or_compaction(leaf.as_deref())
            .unwrap_or_else(|e| panic!("{scenario}: main walk: {e}"));
        assert_eq!(
            entry_ids(&walk),
            stored_ids(&sm_walk),
            "{scenario}: walk parity"
        );
    }
}
