# D-001：session 条目类型单一来源化（合并至 `pir-agent::session`）

> 复制本模板为 `D-NNN-<short-slug>.md` 并填写。登记说明见 [README.md](./README.md)。

- **状态**：已回写
- **关联任务**：T01
- **级别**：实现细节偏离
- **发现日期**：2026-07-30

## 原文档约定

- 文档与章节：`docs/02-design.md` §12（关键模块映射表）、`docs/coding-standards.md` §2.3（文件级对应关系）
- 原文约定：`packages/coding-agent/src/core/session-manager.ts` → `crates/pir/src/core/session_manager.rs`；`packages/agent/src/harness/*` → `crates/pir-agent/src/harness/*`；「一个上游文件对应一个同名 Rust 文件」。

## 实际实现与偏离原因

上游存在两套近乎相同的 session 条目类型定义：coding-agent `session-manager.ts` 的
`SessionHeader` + 9 种 `SessionEntry`，与 harness `types.ts` 的 `SessionTreeEntry`
（多 `active_tools_change` / `leaf`，compaction 多可选 `retainedTail` 且
`firstKeptEntryId` 为可选）。

TS 靠结构类型让两棵类型树各自独立又互操作；Rust 没有结构类型与声明合并，若按文件级
1:1 各定义一份 serde 枚举，两套类型之间必须写显式转换层，且需求 §6.2 本身就把它们
当作一个条目家族（header + 9 + harness 独有 2）描述。

实际实现：两套定义合并为单一来源 `crates/pir-agent/src/session.rs`——每种条目一个
payload struct，`SessionEntry`（11 变体，读路径）、`SessionHeader`、`FileEntry`
（header + 11 变体，原始文件行）三个类型；`CompactionEntry` 同时承载
`firstKeptEntryId` 与 `retainedTail` 两形态（字段均可选，线格式两方向兼容）。
`pir` 主路径（T07）与 harness（T16）共用此模块；「主路径只写 `firstKeptEntryId`」
的钉死版行为由写入方纪律保证（ADR-0003 §1 的措辞不变）。

选择放在 `pir-agent` 而非 `pir`：harness（pir-agent 内）需要这些类型，依赖方向
`pir → pir-agent` 单向不可逆（coding-standards §2.2）。

## 影响面

无（纯内部）。serde 线格式与上游逐字段一致（含 `parentId`/`targetId` 显式 null、
`| undefined` 字段缺省省略），有快照测试覆盖（`pir-agent/src/session.rs` tests）。

## 处置

- **回写位置**：`docs/02-design.md` §12 映射表（session-manager / harness 两行加注）、§4.1 分层表
- **回写日期**：2026-07-30
- **ADR**：不需要
