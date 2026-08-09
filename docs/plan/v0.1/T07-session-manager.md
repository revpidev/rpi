# T07：SessionManager（JSONL 树）

- **状态**：已完成
- **里程碑**：M3
- **依赖**：T01（路径模块）、T05（消息模型）
- **上游对照**：`packages/coding-agent/src/core/session-manager.ts`、`docs/session-format.md`（逐条对拍级基准）
- **需求章节**：§6.1、§6.2、§6.3、§6.4（部分）、§6.6
- **预估**：0.8–1 人月（M3 共 3–3.5，与 T08/T09/T16 合计）

---

## 目标

实现与 Pi 字节兼容的 JSONL session 存储与树导航，保证「能加载并续跑 Pi 生成的
session（v1–v3 自动迁移）」这一成功标准（需求 §1.2.2）。

## 范围

### In

- 统一路径模块（若 T01 未含，本任务落地）：`~/.rpi/agent/sessions/`、目录编码 `--<cwd>--`（**去前导斜杠后 `/`、`\`、`:` → `-`**）、文件名 `<timestamp（:.→-）>_<uuid>.jsonl`、覆盖链 `--session-dir` / `RPI_CODING_AGENT_SESSION_DIR` / `settings.sessionDir`（编码规范 §10.1）
- session 树：`id`（8 hex，randomUUID 前 8 位、碰撞重试 100 次退回完整 UUID）、`parentId`、leaf 分支导航；`getTree` 子节点按 timestamp 排序、孤儿当根
- Header `{type:"session", version, id(uuidv7), timestamp(ISO), cwd, parentSession?}`
- 条目类型全集（header + 9 种，需求 §6.2）：`message` / `model_change` / `thinking_level_change` / `compaction` / `branch_summary` / `custom` / `custom_message` / `label` / `session_info`（serde camelCase，编码规范 §4.4）；`custom` 不进 LLM context、`custom_message` 进
- `compaction` 条目两种形态：`firstKeptEntryId`（主路径，只写这种）/ 内嵌 `retainedTail`（**读取兼容**，harness 产物，ADR-0003 §1）；harness 独有 `active_tools_change` / `leaf` 识别并原样保留
- 加载迁移 v1 → v2（加 id/parentId；`firstKeptEntryIndex` 数字下标 → `firstKeptEntryId`）→ v3（`hookMessage` → `custom`）；迁移后**整文件重写**
- **延迟落盘**：首个 assistant 消息前不创建文件（`flushed` 标志 + `wx` 独占创建）
- **无文件锁**（append 直写；与上游一致——锁仅 auth/settings/trust，G4 红线）
- 读取健壮性：1MB 缓冲流式读、跳过畸形行；header 专用 4KB 缓冲 / 1MB 扫描上限（超限回退全量加载）
- `build_context_entries()` / `build_session_context()`：路径上**最后一个** compaction 生效；输出 = compaction + `firstKeptEntryId` 起 + 其后条目；model 取最后 assistant 或 `model_change`；thinkingLevel 默认 `"off"`
- `createBranchedSession`（抽单路径，**label 剔除并按新 parentId 重链**）、`forkFrom`（新 header + parentSession + 全量原样拷贝 + `wx`；`position: before|at` 与 user-message 校验）
- `appendLabelChange`（空值清除）、`appendSessionInfo`（`\r\n` → 空格 sanitize）
- `--no-session` 内存会话
- 降级策略（需求 §6.6）：未知 / `custom` entry 原样保留、不进 LLM context、写回不丢数据
- import / export JSONL 底层能力（CLI 接线在 T10/T14）

### Out

- compaction 生成逻辑（T08，本任务只含 compaction 条目的读写与 context 重建规则）
- `/tree` `/fork` `/clone` 交互（T12）；`--fork` CLI 在 T10
- harness 的 SessionStorage 实现（T16；trait 设计本任务需与之同构对齐）

## 开发要点

- 线格式 serde 形状逐字段核对 `session-format.md`，用 fixtures 对拍兜底
- 追加写时机与上游一致（message_end / tool / model_change 等事件点）；写失败上抛 `Result`
- retainedTail / `firstKeptEntryId` 两种形态都要能用 Pi fixtures 验证
- **不实现** Pi migrations.ts 的 legacy 启动迁移（ADR-0003 §3，G4 红线）

## 进度跟踪

- [x] 设计细化
- [x] 实现
- [x] 自测
- [x] 门禁验收
- [x] 文档回写

## 自测清单

- [x] v1/v2/v3 fixtures 加载并自动迁移，迁移后结构与上游一致（含 firstKeptEntryIndex 转换）——`core::session_manager::tests`（v1→v3 整文件重写、幂等跳过、未知条目迁移赋 id）
- [x] `build_context_entries` 对 compaction / branch_summary 的裁剪结果与上游一致（两种形态）——合成单测覆盖 `firstKeptEntryId` 与 `retainedTail` 两形态（fixtures 中无 compaction/branch_summary 条目）
- [x] 未知 entry 类型：加载保留 → 写回无损（往返测试）——`unknown_entry_types_roundtrip_losslessly`（含已知条目的未知扩展字段）
- [x] 延迟落盘：首个 assistant 前文件不存在，之后 `wx` 创建——`wx_exclusive_create_fails_when_file_already_exists` 等
- [x] label 重链、forkFrom 拷贝语义、session_info sanitize——`rewires_children_of_removed_labels_when_forking`、fork position before|at 系列、sanitize 单测
- [x] 读取健壮性：畸形行跳过、超大 header 回退——`opens_compatible_sessions_beyond_the_discovery_scan_limit` 等
- [x] 路径覆盖链优先级测试（flag > env > settings > 默认）；目录编码规则（含 Windows 盘符冒号）——`config::tests`
- [x] 追加写失败（只读目录模拟）上抛错误，不 panic——`append_write_failure_in_readonly_directory_is_error_not_panic`

## 门禁验收

通用门禁 G1–G7 全过（G4 重点：仅 JSONL、不读 `~/.pi`、session 无锁；G5 重点：JSONL 形状）。

任务特有标准：

- [x] Pi 生成的 session fixtures 加载 + 续跑（faux）对拍一致——`crates/rpi/tests/parity_session_test.rs`
- [x] 需求 §6.6 三条降级策略各有测试锚点——见验收记录 G3 附表 2
- [x] `session-format.md` 逐条对拍映射表（G3）——见验收记录 G3 附表 1

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| D-012 | SessionManager 与路径模块 Rust 落地差异（合并登记任务内 T07-D1～D9：retainedTail 展开采 session-format.md/harness 行为、随机源自实现、serde default 修正、list/listAll 留 T12、typed 联合体降级边界 4 项、同步 IO 等） | 已回写 |

## 验收记录

- 验收日期：2026-08-03
- 验收人：kimi-code（单人开发，按清单逐项自证；另经一轮独立 fresh-eyes 对拍复核，6 条应修 + 2 条边缘修正已落地，见下）
- G1 构建/静态检查：通过（`cargo build --workspace`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo fmt --all -- --check` 全绿，无警告）
- G2 测试：通过（`cargo test --workspace` 27 个 test binary 全 ok，0 failed；rpi crate lib 224 passed（含 `core::session_manager::tests` 118 例 + `config::tests`）、parity_session 2 例；rpi-ai lib 327 passed（含 uuid 3 例与 provider_retry PRNG 回归 1 例）；无 live 测试，非 live 不访问网络）
- G3 对拍：通过。`cargo test -p rpi --test parity_session_test`：5 个 fixtures 场景（abort / length-truncation / single-turn / steering-followup / tool-calls）真实 Pi 落盘 session.jsonl 逐个 `SessionManager::open` 加载 + `export_jsonl()` 写回，经 `rpi_test_support::diff::diff_jsonl`（内含 Normalizer）归一化 diff 一致（`parity_fixture_sessions_load_and_export_lossless`）；加载后续跑追加 user + faux assistant 消息，context 旧消息不变、文件恰增 2 行、重开状态完整（`parity_fixture_sessions_continue_after_load`）。`session-format.md` 逐条映射见附表 1；需求 §6.6 降级策略锚点见附表 2
- G4 红线：通过（`external/pi` `git status --porcelain` 为空、HEAD=2efa728；无 JS/TS 执行能力；未读写 `~/.pi`/`.pi`（路径模块根为 ADR-0001 的 `~/.rpi`）；Session 仅 JSONL；token 估算未触碰；非测试代码无 unwrap/expect；日志无凭据；无范围排除项引入（无 legacy 启动迁移）；无 rg/fd 下载机制；session 写入无文件锁——append 直写，与上游一致）
- G5 线格式：通过（条目类型 camelCase serde 形状锚定在 `rpi-agent/src/session.rs` T01 形状测试（header/9 种主路径条目/harness 2 种/compaction 双形态逐字段断言）；本任务新增 `StoredEntry` Raw 保留走 `serde_json::Value` 原样往返；fixtures 写回 diff 与 G3 合并执行）
- G6 文档同步：通过（移植文件均带上游路径+版本溯源注释；回写 `02-design.md` §6.3 Rust 落地注记、§8 路径模块落地说明，`01-requirements.md` §6.6 降级策略边界注记）
- G7 偏离闭环：通过（D-012 已登记至 `deviations/` 并回写，状态「已回写」；任务内 T07-D1～D9 全部并入 D-012，无未闭环项。T07-D1 经核实不定行为级：主路径只写 firstKeptEntryId 形态、fixtures 无 compaction 条目，对拍契约不受影响）
- 结论：通过

### G3 附表 1：`session-format.md` 逐条对拍映射表

| 基准条目（session-format.md） | 测试锚点 |
|------------------|----------|
| File Location（目录编码 `--<cwd>--`、文件名 `<timestamp>_<uuid>.jsonl`、覆盖链） | `config::tests`（含 Windows 盘符冒号、空串落空、覆盖优先级）；`session_file_name_replaces_colons_and_dots` |
| Session Version（v1–v3、CURRENT_SESSION_VERSION=3） | `should_add_id_parent_id_to_v1_entries`、`should_be_idempotent_skip_already_migrated`、`converts_first_kept_entry_index_to_first_kept_entry_id`、`renames_hook_message_role_to_custom`、`migrated_v1_file_is_rewritten_on_open`、`migrate_v1_removes_float_first_kept_entry_index`、`migrate_float_version_2_0_is_not_treated_as_v1` |
| Message Types / AgentMessage Union | T01 形状测试（`rpi-agent/src/session.rs`：message/bashExecution roundtrip）+ fixtures 加载对拍 |
| Entry Base（id/parentId/timestamp） | `append_message_creates_entry_with_correct_parent_id_chain`、`leaf_pointer_advances_after_each_append`、`entry_ids` |
| SessionHeader | T01 `session_header_shape`（v1 无 version 键、parentSession 省略）；`reads_cwd_from_session_*` 系列 |
| SessionMessageEntry | T01 `message_entry_shape_with_null_parent`；`append_message_creates_entry_with_correct_parent_id_chain` |
| ModelChangeEntry | T01 `remaining_entry_type_shapes`；`append_model_change_integrates_into_tree`、`tracks_model_from_model_change_entry` |
| ThinkingLevelChangeEntry | T01 `remaining_entry_type_shapes`；`append_thinking_level_change_integrates_into_tree`、`tracks_thinking_level_changes` |
| CompactionEntry（firstKeptEntryId + retainedTail 双形态） | T01 `compaction_entry_first_kept_entry_id_form` / `compaction_entry_retained_tail_form`；`includes_summary_before_kept_messages`、`handles_compaction_keeping_from_first_message`、`multiple_compactions_uses_latest`、`retained_tail_compaction_form_is_self_contained_checkpoint`（D-012 第 1 条） |
| BranchSummaryEntry | T01 `remaining_entry_type_shapes`；`includes_branch_summary_in_path`、`branch_with_summary_inserts_branch_summary_and_advances_leaf` |
| CustomEntry（不进 LLM context） | T01 `remaining_entry_type_shapes`；`append_custom_entry_integrates_into_tree`、`saves_custom_entries_and_includes_them_in_tree_traversal`、`build_context_entries_returns_compaction_aware_entries_including_custom_entries` |
| CustomMessageEntry（进 context） | T01 `remaining_entry_type_shapes` + `session.rs` content serde default（D-012 第 3 条） |
| LabelEntry | T01 `remaining_entry_type_shapes`（label None 省略 key）；`sets_and_gets_labels`、`clears_labels_with_none`、`last_label_wins`、`labels_are_included_in_tree_nodes`、`append_label_change_throws_when_labeling_nonexistent_entry` |
| SessionInfoEntry | T01 `remaining_entry_type_shapes`；`append_session_info_sanitizes_newlines`、`get_session_name_uses_latest_entry_and_honors_clears` |
| Tree Structure（getTree 子节点按 timestamp、孤儿当根） | `get_tree_*` 7 例（含 `get_tree_treats_orphans_as_roots_and_sorts_children_by_timestamp`、`get_tree_handles_deep_branching`） |
| Context Building（最后 compaction 生效、firstKeptEntryId 起裁剪、branch_summary、custom 不进/custom_message 进） | `build_context_*`/`includes_*`/`complex_tree_with_multiple_branches_and_compaction`/`keeps_settings_from_the_full_path_after_compaction`/`path_to_root_or_compaction_stops_at_compaction_checkpoint` 等 12 例 |
| Parsing Example（加载语义） | `load_entries_*` 6 例 + `parity_fixture_sessions_load_and_export_lossless` |
| SessionManager API（create/open/append/tree/context/fork/branched/label/info） | `create_branched_session_*` 5 例、`fork_from_*` 4 例、`branch_*` 4 例、`get_branch_*` 5 例、`get_entry_*`/`get_leaf_entry_*` 4 例、`set_session_file_*` 4 例、`continue_recent_opens_most_recent_or_creates_new`、`in_memory_session_never_touches_disk`；延迟落盘 `deferred_persistence_no_file_before_first_assistant`、`wx_exclusive_create_fails_when_file_already_exists` |
| （无对应基准节）读取健壮性 | `malformed_lines_are_skipped_and_session_stays_usable`、`find_most_recent_session_skips_oversized_corrupt_files`、`reads_cwd_from_session_with_multi_buffer_header` |
| （无对应基准节）无损往返 | `unknown_entry_types_roundtrip_losslessly`、`migration_preserves_unknown_entries_while_assigning_ids`、`migration_rewrite_preserves_extra_fields_on_known_entries`、`branch_and_fork_preserve_unknown_extension_fields` |
| （无对应基准节）写失败上抛 | `append_write_failure_in_readonly_directory_is_error_not_panic` |
| （无对应基准节）label 重放确定性 | `branched_session_replays_labels_in_insertion_order`（独立复核修正项） |

### G3 附表 2：需求 §6.6 降级策略锚点

| 降级策略 | 测试锚点 |
|----------|----------|
| 保留（未知/custom entry 原样保留、写回不丢数据） | `unknown_entry_types_roundtrip_losslessly`、`branch_and_fork_preserve_unknown_extension_fields`、`migration_rewrite_preserves_extra_fields_on_known_entries`、parity 写回 diff；边界注记 D-012 第 5/8/9 条 |
| 跳过 LLM context（custom 不进、custom_message 进） | `build_context_entries_returns_compaction_aware_entries_including_custom_entries`、`labels_are_not_included_in_build_session_context` |
| 通用渲染（TUI 未知 custom 通用块） | T12 范畴，本任务不涉及（SessionManager 层保证 Raw 保留供渲染层消费） |

### 复核修正记录（验收过程中发现并已修复）

- `rpi-ai/src/utils/uuid.rs` 与 `provider_retry.rs` 的 xorshift 播种 bug（`compare_exchange` Ok 旧值 0 被当作种子，随机源恒零）——两处修复 + `test_random_unit_is_not_constant_zero` 回归锚点
- 已知条目未知扩展字段在 branch/fork 路径丢失（改从 raw 出发改 parentId）；label 重放 HashMap 顺序不确定（改插入序 Vec）；`parse_session_entries` 误滤 header（删除对齐上游）；`get_branch` 未知 id 回退（改为返回空）；`fork_from` 校验先于 wx 写（不留孤儿文件）；覆盖链空串逐级落空；v1 迁移 `firstKeptEntryIndex` 任意 JSON number 均删除、浮点 version 不当 v1
