# T16：pir-agent harness 层

- **状态**：未开始
- **里程碑**：M3
- **依赖**：T05（Agent/loop）、T07（session 存储与条目格式）、T08（compaction 共享常量）
- **上游对照**：`packages/agent/src/harness/*`（agent-harness.ts、types.ts、session/*、compaction/*、skills.ts、prompt-templates.ts、system-prompt.ts、tools/*、messages.ts（其 convertToLlm 与 4 个 summary 包裹常量归 T05 主线）、env/、utils/）、`packages/agent/src/proxy.ts`、`docs/agent-harness.md`
- **需求章节**：§4.4；§6.2（retainedTail 与 harness 独有条目）
- **预估**：1–1.5 人月（M3 共 3–3.5，与 T07/T08/T09 合计）

---

## 目标

完整移植 agent 包的 harness 层（ADR-0003 §1）：AgentHarness、SessionStorage/Repo 抽象、
harness 事件与持久化屏障，使 `pir-agent` 对 SDK 嵌入方提供与 Pi 同构的可选层，
并保证 harness 产物会话文件与 `pir` 主路径互通。

## 范围

### In

- `AgentHarness`：phase 状态机（idle/turn/compaction/branch_summary/retry）；**turn snapshot vs config 分离**（setters 立即生效但只影响下一 turn 快照）；三队列（steer / followUp / **nextTurn**，nextTurn 于下次 prompt 并入头部）；错误归一化（`AgentHarnessError` 等结构化错误码；**能力层不抛异常、错误走 Result**）
- **harness 事件 22 种**（需求 §4.4 清单）与各 hook 返回类型映射
- **持久化屏障（决定 JSONL 行序，对拍核心）**：`message_end` 先写 session 再发事件；busy 期间写入进 `pending_session_writes`；`turn_end` flush 后 emit `save_point`；`agent_end` flush + phase→idle + `settled`；**失败路径**：失败使 loop reject → `emitRunFailure` 合成失败消息重放完整事件序列；二次失败聚合 `AgentHarnessError`；finally flush 失败直接抛出（agent-harness.ts:486-655）
- 队列语义：steer/followUp 在 idle 时抛错；`abort()` 清两队列 + 聚合 queue_update/waitForIdle/abort 事件错误；drain 失败 requeue（queue_update hook 抛错放回队头）
- `compact()`：`session_before_compact` 字段级语义 `{cancel?, compaction?}`（完整 CompactionResult 即接管并打 fromHook 标记）；entry 带 `fromHook` 标记
- `navigateTree()`：目标为 user/custom_message 时 newLeaf 指向 parentId 并返回 editorText；`session_before_tree` 字段级语义 `{cancel?, summary?（仅 summarize 模式采用）, customInstructions?, replaceInstructions?, label?}`
- `session_before_fork` 字段级语义 `{cancel?, skipConversationRestore?}`：上游仅 cancel 生效、skipConversationRestore 为 reserved 未实现字段——pir 只实现 cancel 并登记差异
- `SessionStorage` / `SessionRepo` trait + `JsonlSessionStorage`（header **version: 3**（硬校验：非 v3 抛 `invalid_session`，不做 v1/v2 迁移——迁移是主路径 SessionManager 独有行为，jsonl-storage.ts:17,77）；entry id=uuidv7 后 8 位碰撞重试；**`firstKeptEntryId` 与 `retainedTail` 两形态读写**；harness 独有 `active_tools_change` / `leaf` 条目）+ `JsonlSessionRepo`（`--<cwd>--` 编码、`<timestamp>_<id>.jsonl`）+ `InMemory*`；trait 与 T07 SessionManager 同构对齐（SQLite 不做，ADR-0002 §7）
- harness 自带模块（**复用 T08/T09 的共享常量与算法**：token 估算、summary prompt、切点、截断常数）：
  - compaction（split-turn 双段摘要、`<read-files>`/`<modified-files>`、请求隔离）
  - branch summarization（预算 contextWindow−16384、0.9 阈值强留、maxTokens 2048）
  - skills 加载（ignore 链、name 校验仅警告、`formatSkillsForSystemPrompt` XML + escapeXml、`formatSkillInvocation`）
  - prompt-templates（`$N`/`$@`/`$ARGUMENTS`/`${@:N}`/`${@:N:L}`、description 首行 60 字符回退）
  - 默认工具工厂（read/write/edit/bash：2000 行/50KB head 截断、5 种图片格式、edit fuzzy+BOM/行尾保留、bash tail 截断+100ms 节流+temp 文件、`withFileMutationQueue` env+canonical path 串行化）
- **订阅模型与扩展点**：subscribe/on 双订阅模型（subscribe 纯观察、`*` 通配；on 带返回值、多 handler 顺序执行最后非 undefined 胜出；patch 型 hook 归约）；`entryTransforms`/`entryProjectors` 扩展点；leaf 追加+重放重建语义
- `stream_proxy`：SSE 客户端协议（POST `/api/stream`，服务端剥离 partial、客户端重建）；proxy 12 种事件类型：start/text_start/text_delta/text_end/thinking_start/thinking_delta/thinking_end/toolcall_start/toolcall_delta/toolcall_end/done/error
- **行为差异登记**：harness 工具/资源实现与 coding-agent 版的差异以 coding-agent 为对拍基准（ADR-0003 §2），差异点逐条登记到 `deviations/` 或注释标注

### Out

- `pir` CLI 主路径不经过 harness（与 Pi 一致；无接线工作）
- harness 的 SQLite 后端（永久非目标，ADR-0002 §7）
- harness 专属交互 UI（无对应物）

## 开发要点

- **以钉死 commit 的代码行为为准**（harness 自述「生命周期仍在硬化中」，不以其设计文档为准；设计原则 4）
- 移植上游 harness 相关测试意图，同名 Rust 测试
- 与 T07 的格式对齐是互通关键：同一 JSONL fixtures 两实现（SessionManager / JsonlSessionStorage）都能读写，交叉验证
- harness 会话产物（含 retainedTail、active_tools_change、leaf）须能被 T07 主路径加载保留（需求 §6.6）

## 进度跟踪

- [ ] 设计细化
- [ ] 实现
- [ ] 自测
- [ ] 门禁验收
- [ ] 文档回写

## 自测清单

- [ ] 上游 harness 测试意图移植通过（事件序、phase 迁移、屏障语义逐条断言）
- [ ] 持久化屏障：message_end 先写后发；pending 队列在 turn_end/agent_end 的 flush 顺序；JSONL 行序与上游 fixtures 一致；失败路径：emitRunFailure 合成失败消息重放完整事件序列、二次失败聚合 AgentHarnessError、finally flush 失败直接抛出
- [ ] 三队列语义：nextTurn 并入、idle 抛错、abort 聚合、drain 失败 requeue
- [ ] compact / navigateTree：hook cancel/接管、fromHook 标记、editorText 回填
- [ ] retainedTail 读写往返；`active_tools_change` / `leaf` 条目写读
- [ ] 互通：harness 产物 fixtures 被 T07 SessionManager 加载保留、续跑（faux）不丢数据；T07 产物被 harness 加载
- [ ] harness compaction/branch-summary 与 T08 共享常量一致（黄金用例复用）
- [ ] stream_proxy 协议契约测试（构造 SSE 流）

## 门禁验收

通用门禁 G1–G7 全过（G3 以 harness fixtures + 互通对拍执行；G4 重点：仅 JSONL）。

任务特有标准：

- [ ] 需求 §4.4 各条目（状态机/事件/屏障/存储/各子模块）逐条核对有测试锚点（验收记录列映射表）
- [ ] 主路径（T07）↔ harness 双向互通对拍通过
- [ ] harness 与 coding-agent 行为差异清单完成并登记

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| — | （暂无） | — |

## 验收记录

（待填写，模板见 `gates.md` §3）
