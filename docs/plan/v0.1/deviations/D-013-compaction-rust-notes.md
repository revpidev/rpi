# D-013：compaction 移植 Rust 落地差异（模块落点 / StreamOptions.reasoning / 共享函数下沉）

> 复制本模板为 `D-NNN-<short-slug>.md` 并填写。登记说明见 [README.md](./README.md)。

- **状态**：已回写
- **关联任务**：T08
- **级别**：实现细节偏离
- **发现日期**：2026-08-03

## 原文档约定

- 文档与章节：`docs/02-design.md` §6.4、§12（模块映射表）、§4.4（StreamFn 注入）
- 原文约定：
  - §12 映射表：`packages/coding-agent/src/core/compaction/*` → `crates/pir/src/core/compaction/*`；
  - §4.4：`StreamFn = Arc<dyn Fn(Model, Context, StreamOptions) -> BoxStream<..>>`，
    `StreamOptions` 不含 reasoning 通道（上游 reasoning 住在 `SimpleStreamOptions`，
    D-010 注记称 reasoning/thinking_budgets 保留在 `AgentLoopConfig` 由组装层绑定）。

## 实际实现与偏离原因

1. **算法层落点为 `pir-agent::compaction`，而非 `pir::core::compaction`**。
   设计 §4.5 已规定 compaction/branch-summary 的算法与常量「抽到 `pir-agent` 公共模块
   供 coding-agent 与 harness（T16）两处复用」，§12 映射表未同步这一层。落地拆分：
   - `crates/pir-agent/src/compaction.rs`（+ `compaction/utils.rs`、`compaction/branch_summarization.rs`）：
     `compaction.ts` / `utils.ts` / `branch-summarization.ts` 的逐字节移植（估算、切点、
     prompt 模板、summary 生成、branch 装填），coding-agent 与 T16 harness 共用；
   - `crates/pir/src/core/compaction_runner.rs`：仅 coding-agent 侧触发接线
     （`_checkCompaction` 双路、overflow 一次恢复、`_runAutoCompaction`、事件发射），
     即原映射表 §12 对应物的实际位置。
2. **`pir-ai::types::StreamOptions` 增加 `reasoning: Option<ModelThinkingLevel>` 字段**。
   上游 summary 调用走 `complete_simple` 经 `SimpleStreamOptions.reasoning` 传 thinking
   level（`createSummarizationOptions`，compaction.ts:539-553）；pir-agent 的 summary
   生成直接走 §4.4 的 `StreamFn`（参数为裸 `StreamOptions`），reasoning 通道只能落在
   `StreamOptions` 上。`SimpleStreamOptions.reasoning` 保留不动（`stream_simple` 路径
   语义不变）；`StreamOptions.reasoning` 仅由 compaction 调用方写入，provider 适配层
   读取优先级与上游 `SimpleStreamOptions` 一致。
3. **session 条目→context 消息的共享函数下沉**：
   `parse_iso8601_ms` / `session_entry_to_context_messages` / `get_latest_compaction_entry`
   / `build_context_messages` 落在 `pir-agent::session`，`pir::core::session_manager`
   改为 `pub use` re-export（D-001 单一来源原则的延伸；harness 层 T16 也需要同一实现）。

## 影响面

无（纯内部）。不改变 session JSONL 格式、事件形状、prompt 字节或任何对拍契约；
`StreamOptions.reasoning` 默认 `None`，既有调用路径行为不变。

## 处置

- **回写位置**：`docs/02-design.md` §6.4（模块落点注记）、§4.4（StreamOptions.reasoning
  注记）、§12（映射表 compaction 行）
- **回写日期**：2026-08-03
- **ADR**：不需要
