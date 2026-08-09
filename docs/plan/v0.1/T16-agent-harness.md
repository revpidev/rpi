# T16：rpi-agent harness 层

- **状态**：已完成
- **里程碑**：M3
- **依赖**：T05（Agent/loop）、T07（session 存储与条目格式）、T08（compaction 共享常量）
- **上游对照**：`packages/agent/src/harness/*`（agent-harness.ts、types.ts、session/*、compaction/*、skills.ts、prompt-templates.ts、system-prompt.ts、tools/*、messages.ts（其 convertToLlm 与 4 个 summary 包裹常量归 T05 主线）、env/、utils/）、`packages/agent/src/proxy.ts`、`docs/agent-harness.md`
- **需求章节**：§4.4；§6.2（retainedTail 与 harness 独有条目）
- **预估**：1–1.5 人月（M3 共 3–3.5，与 T07/T08/T09 合计）

---

## 目标

完整移植 agent 包的 harness 层（ADR-0003 §1）：AgentHarness、SessionStorage/Repo 抽象、
harness 事件与持久化屏障，使 `rpi-agent` 对 SDK 嵌入方提供与 Pi 同构的可选层，
并保证 harness 产物会话文件与 `rpi` 主路径互通。

## 范围

### In

- `AgentHarness`：phase 状态机（idle/turn/compaction/branch_summary/retry）；**turn snapshot vs config 分离**（setters 立即生效但只影响下一 turn 快照）；三队列（steer / followUp / **nextTurn**，nextTurn 于下次 prompt 并入头部）；错误归一化（`AgentHarnessError` 等结构化错误码；**能力层不抛异常、错误走 Result**）
- **harness 事件 22 种**（需求 §4.4 清单）与各 hook 返回类型映射
- **持久化屏障（决定 JSONL 行序，对拍核心）**：`message_end` 先写 session 再发事件；busy 期间写入进 `pending_session_writes`；`turn_end` flush 后 emit `save_point`；`agent_end` flush + phase→idle + `settled`；**失败路径**：失败使 loop reject → `emitRunFailure` 合成失败消息重放完整事件序列；二次失败聚合 `AgentHarnessError`；finally flush 失败直接抛出（agent-harness.ts:486-655）
- 队列语义：steer/followUp 在 idle 时抛错；`abort()` 清两队列 + 聚合 queue_update/waitForIdle/abort 事件错误；drain 失败 requeue（queue_update hook 抛错放回队头）
- `compact()`：`session_before_compact` 字段级语义 `{cancel?, compaction?}`（完整 CompactionResult 即接管并打 fromHook 标记）；entry 带 `fromHook` 标记
- `navigateTree()`：目标为 user/custom_message 时 newLeaf 指向 parentId 并返回 editorText；`session_before_tree` 字段级语义 `{cancel?, summary?（仅 summarize 模式采用）, customInstructions?, replaceInstructions?, label?}`
- `session_before_fork` 字段级语义 `{cancel?, skipConversationRestore?}`：上游仅 cancel 生效、skipConversationRestore 为 reserved 未实现字段——rpi 只实现 cancel 并登记差异
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

- `rpi` CLI 主路径不经过 harness（与 Pi 一致；无接线工作）
- harness 的 SQLite 后端（永久非目标，ADR-0002 §7）
- harness 专属交互 UI（无对应物）

## 开发要点

- **以钉死 commit 的代码行为为准**（harness 自述「生命周期仍在硬化中」，不以其设计文档为准；设计原则 4）
- 移植上游 harness 相关测试意图，同名 Rust 测试
- 与 T07 的格式对齐是互通关键：同一 JSONL fixtures 两实现（SessionManager / JsonlSessionStorage）都能读写，交叉验证
- harness 会话产物（含 retainedTail、active_tools_change、leaf）须能被 T07 主路径加载保留（需求 §6.6）

## 进度跟踪

- [x] 设计细化
- [x] 实现
- [x] 自测
- [x] 门禁验收
- [x] 文档回写

## 设计细化记录（2026-08-06）

### 模块映射（上游 → `crates/rpi-agent/src/`）

| 上游（`packages/agent/src/`） | 落地 |
|---|---|
| `harness/types.ts` | `harness/types.rs`（错误族、phase、22 自有事件 + hook 结果映射、SessionStorage/SessionRepo trait、FileSystem/Shell/ExecutionEnv trait、配置选项） |
| `harness/agent-harness.ts` | `harness/agent_harness.rs` |
| `harness/session/session.ts` | `harness/session.rs`（Session 门面；条目 serde 类型单一来源仍在 crate 根 `session.rs`，D-001） |
| `harness/session/{jsonl-storage,jsonl-repo,memory-storage,memory-repo,repo-utils}.ts` | `harness/session/{jsonl_storage,jsonl_repo,memory_storage,memory_repo,repo_utils}.rs` |
| `harness/compaction/*` | LLM 调用/估算/切点/文件操作**复用** crate 根 `compaction/`；**勘误（D-020）**：`prepareCompaction` harness 变体与 coding-agent 版不同（不提前返回、带 retainedTail），`prepare_harness_compaction` 落 `agent_harness.rs` |
| `harness/messages.ts` | **复用** crate 根 `messages.rs`（convert_to_llm 与 4 个 summary 包裹常量已在 T05 落地） |
| `harness/{skills,prompt-templates,system-prompt}.ts` | `harness/{skills,prompt_templates,system_prompt}.rs`（**独立移植**：上游 harness 版与 coding-agent 版本身就是双份、文案/签名不同；rpi-agent 不能依赖 rpi） |
| `harness/tools/*` | `harness/tools/*`（read/write/edit/edit_diff/bash/image/file_mutation_queue/path_utils/tool_context，走 ExecutionEnv 抽象） |
| `harness/env/nodejs.ts` | `harness/env/nodejs.rs`（tokio 原生实现 `NodeExecutionEnv`；FileSystem/Shell/ExecutionEnv trait 在 `harness/types.rs`） |
| `harness/utils/{truncate,shell-output}.ts` | `harness/utils/{truncate,shell_output}.rs` |
| `proxy.ts` | crate 根 `proxy.rs`（**勘误（D-020）**：rpi-ai SSE 解码器语义不同（空行派发+event 字段），proxy 按 `data: ` 逐行解析自行实现；HTTP 用 reqwest） |

既有 `src/harness.rs`（T07 期的 SessionStorage trait 骨架，159 行）扩为 `harness.rs` + `harness/` 子模块目录（Rust 2018 路径风格，不用 mod.rs）。

### 关键决策

1. **依赖新增（rpi-agent）**：`reqwest`（proxy）、`serde_yaml` + `ignore`（skills/templates 加载，与 rpi 同版本同语义）、`unicode-normalization`（edit fuzzy / 路径变体）、`base64`（read 图片）、`libc`（进程树击杀）。均已在 workspace 基线（coding-standards 附录 A）内。
2. **id/格式**：harness `createEntryId` = uuidv7 **后 8 位** + 碰撞重试 100 次回退完整 uuid（复用 `rpi_ai::utils::uuid::uuidv7`）；与 T07 主路径（随机 UUID 前 8 位）不同属上游真实差异，不算偏离。header version!==3 硬校验抛 `invalid_session`，**不做迁移**（迁移是主路径独有）。
3. **hook 模型**：`subscribe` = 纯观察广播（含低层 AgentEvent，无返回）；`on` = 按事件注册、顺序执行、最后非 None 结果胜出；结果用 `HarnessHookResult` 枚举承载各事件返回类型；`before_provider_request`/`before_provider_payload` 为链式变换（patch 支持删除语义）。hook 抛错归一化 `hook` 错误码。
4. **phase 机**：`"retry"` 在上游类型里存在但从未赋值（vestigial）——Rust 侧保留枚举变体并注释说明，不实现迁移。
5. **错误走 Result**：能力层不 panic；`AgentHarnessError`/`SessionError`（已有）/`CompactionError`/`BranchSummaryError`/`FileError`/`ExecutionError` 按上游 code 字面值。
6. **测试**：移植上游 14 个 harness 测试文件意图（sqlite-* 两个除外，测的是排除范围内的 sqlite-node 包）；互通对拍：harness JSONL 产物 ↔ T07 SessionManager 双向加载。

## 自测清单

- [x] 上游 harness 测试意图移植通过（事件序、phase 迁移、屏障语义逐条断言）——`tests/agent_harness_test.rs` 23 + `agent_harness_stream_test.rs` 4 + session 内联 74（storage 40 + facade 34）+ tools 41 + nodejs_env 29 + resources 24 + truncate 17 + proxy 16
- [x] 持久化屏障：message_end 先写后发；pending 队列在 turn_end/agent_end 的 flush 顺序；JSONL 行序与上游 fixtures 一致；失败路径：emitRunFailure 合成失败消息重放完整事件序列、二次失败聚合 AgentHarnessError、finally flush 失败直抛——`agent_harness_test.rs`（pending 写排序、save-point 五件套、hook 失败落成持久化错误消息、`test_failed_failure_reporting_aggregates_unknown_error`、`test_finally_flush_failure_overrides_run_failure`）
- [x] 三队列语义：nextTurn 并入、idle 抛错、abort 聚合、drain 失败 requeue——`agent_harness_test.rs`（steer [1,2,1,0]、abort 保 nextTurn、followUp 排空、`test_drain_failure_requeues_steer_message_and_fails_run`、`test_steer_and_follow_up_rejected_while_idle_next_turn_queueable`）
- [x] compact / navigateTree：hook cancel/接管、fromHook 标记、editorText 回填——`agent_harness_test.rs` compaction/branch-summary usage 各 2 例 + navigateTree 用例
- [x] retainedTail 读写往返；`active_tools_change` / `leaf` 条目写读——session 内联测试 + 互通对拍 2 号用例
- [x] 互通：harness 产物 fixtures 被 T07 SessionManager 加载保留、续跑（faux）不丢数据；T07 产物被 harness 加载——`rpi/tests/parity_harness_interop_test.rs` 4 用例（修复 1 个真实分歧：build_index leaf 重放，D-020 第 3 条）
- [x] harness compaction/branch-summary 与 T08 共享常量一致（黄金用例复用）——T08 黄金 16 例锁常量；变体差异（D-020 第 1 条）由 `test_compaction_persists_generated_usage`（retainedTail=2 断言）锚定
- [x] stream_proxy 协议契约测试（构造 SSE 流）——`tests/proxy_test.rs` 12 用例（本地脚本服务器）+ 内联 4

## 门禁验收

通用门禁 G1–G7 全过（G3 以 harness fixtures + 互通对拍执行；G4 重点：仅 JSONL）。

任务特有标准：

- [x] 需求 §4.4 各条目（状态机/事件/屏障/存储/各子模块）逐条核对有测试锚点（验收记录列映射表）
- [x] 主路径（T07）↔ harness 双向互通对拍通过
- [x] harness 与 coding-agent 行为差异清单完成并登记（D-020 第 4/7 条 + 各模块头注「Intentional differences」）

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| D-020 | harness 层 Rust 落地差异（compaction 变体勘误、SessionStorage &self+Mutex、build_index leaf 重放兼容、resources 独立移植、依赖落位、8 组局部等价） | 已回写 |

## 验收记录

- 验收日期：2026-08-06
- 验收人：kimi-code（单人开发，按清单逐项自证，命令实跑）
- G1 构建/静态检查：通过（`cargo build --workspace` / `cargo clippy --workspace --all-targets -- -D warnings` / `cargo fmt --all -- --check` 全部零警告）
- G2 测试：通过（`cargo test --workspace` **2879 passed, 0 failed**；无 live 测试、非 live 测试不触真实网络——proxy 契约测试走本地 TcpListener 脚本服务器）
- G3 对拍：通过
  - 互通对拍 `rpi/tests/parity_harness_interop_test.rs` 4 用例：harness→主路径（11 种条目全保留 + firstKeptEntryId 与 retainedTail 两形态 + 收尾 leaf 移动 + `export_jsonl()` 字节级相等 + 续跑）、主路径→harness（repo open + 行走逐 id 相等 + stats/name/label/context 一致）、fixtures 交叉（7 个场景 session.jsonl 被 harness JsonlSessionStorage 全量加载、version 3 硬校验、无降级）
  - fixtures 黄金：T08 compaction 黄金（`compaction_golden_test.rs` 16 例）锁定共享常量/算法，harness 侧复用同一模块；harness 变体差异由 `agent_harness_test.rs::test_compaction_persists_generated_usage`（含 retainedTail=2 断言）锚定
  - edit-diff 11 组 jsdiff 8.x 逐字节 golden（patch/diff/firstChangedLine）
- G4 红线：通过（`external/pi` `git status --porcelain` 为空且 HEAD=`2efa728`；未引入 JS 执行/SQLite/rg|fd 下载；未读写 `~/.pi`/`.pi`；session 写入无文件锁（grep 验证 rpi-agent 无 fs2）；新增非测试代码无 `unwrap()`/`expect()`（脚本逐文件扫描 cfg(test) 之前区段，零命中）；proxy Bearer token 不入日志；token 估算复用 T08 模块未偏离；新增依赖 6 项均在编码规范附录 A 基线内，落位 rpi-agent 已登记 D-020）
- G5 线格式：通过（22 事件 serde `tag="type"` snake_case + camelCase 字段、错误 code 字面值、session 条目/header 形状——types.rs 12 个 serde 锚点测试 + 互通对拍字节级比对覆盖）
- G6 文档同步：通过（全部移植文件带 §14.3 溯源头 + 行级注释；D-020 回写 `02-design.md` §6.4（compaction 变体勘误补记）与 §12（harness 映射行）；本任务文件设计细化记录）
- G7 偏离闭环：通过（D-020 一条，实现细节级，状态「已回写」，无需 ADR）
- 结论：**通过**

任务特有标准——需求 §4.4 逐条映射表：

| §4.4 条目 | 测试锚点 |
|-----------|----------|
| phase 状态机（idle/turn/compaction/branch_summary；retry 为上游 vestigial） | `agent_harness_test.rs` 构造/busy 校验用例；types.rs phase 测试 |
| turn snapshot vs config 分离（setters 只影响下一 turn） | `agent_harness_stream_test.rs` 快照先于 provider hook / save-point 新配置不改在途请求 |
| 三队列（nextTurn 并入头部、idle 抛错、abort 清两队保 nextTurn、drain 失败 requeue） | `agent_harness_test.rs` steer 排空 [1,2,1,0] / abort 保 nextTurn / followUp 排空 / queue_update 回滚 |
| 错误归一化（9 码 AgentHarnessError 等、能力层走 Result） | types.rs 错误族字面值测试；hook 失败→`hook` 码用例 |
| 22 事件 + subscribe/on 双订阅（最后非 None 胜出、patch 归约） | types.rs 22 事件 serde 测试；`agent_harness_test.rs` hook 系列；`agent_harness_stream_test.rs` patch 链式+删除语义 |
| 持久化屏障（message_end 先写后发、turn_end flush→save_point、agent_end flush→idle→settled、finally flush） | `agent_harness_test.rs` pending 写排序 / save-point 五件套 / waitForIdle |
| 失败路径（emitRunFailure 重放四事件、二次失败聚合、finally flush 失败直抛） | `agent_harness_test.rs` hook 失败落成持久化错误消息 |
| SessionStorage/SessionRepo 抽象 + JSONL v3 硬校验 + InMemory | `harness/session/*` 内联 40 测试（storage.test.ts/repo.test.ts 意图全移植） |
| entry id=uuidv7 后 8 位 + 碰撞重试 | repo_utils 测试 + jsonl_storage 测试 |
| entryTransforms/entryProjectors + custom 默认不投影 | `session_facade.rs` 34 测试（session.test.ts 意图，双存储参数化） |
| leaf 追加 + 重放重建 | jsonl_storage/memory_storage leaf 用例 + 互通对拍 leaf 断言 |
| harness compaction/branch summary（复用 T08 常量 + 变体勘误） | T08 黄金 16 例 + compaction usage 持久化 2 例 + branch summary 2 例 |
| skills / prompt-templates / system-prompt（harness 版） | skills 11 + templates 10 + system-prompt 3 + resource-formatting（内联） |
| 默认工具工厂（read/write/edit/bash + mutation queue） | `harness/tools/*` 41 测试（tools.test.ts 意图全移植） |
| ExecutionEnv（FileSystem+Shell 原生实现） | `tests/nodejs_env_test.rs` 29 + shell_output 5 |
| truncate（2000 行/50KB、UTF-8 边界） | truncate 17 测试（含确定性 fuzz 对拍） |
| stream_proxy（12 事件、partial 重建、/api/stream） | proxy.rs 内联 4 + `tests/proxy_test.rs` 12 契约用例 |

### 2026-08-06 审查修复记录（验收后追加）

逐行审查（3 reviewer 子代理 + 人工核心文件审查）确认：无严重问题；中等问题 6 项与轻微问题全部处置——
read.rs i64 溢出（saturating + 7 组对抗用例）、edit_diff Myers 线性空间化（~1.1GB→~4MB，逐字节一致由 golden + 132,496 穷举对锁定）、file_mutation_queue Box::leak 泄漏（Arc 注册表按需 GC）、session_facade append 非原子（append_lock + 并发链完整性测试）、失败路径测试缺口（补 drain requeue / idle 抛错 / 二次失败聚合 / flush 失败覆盖 4 例）、harness 测试全量 timeout 护栏（27 例）、save_point 信号对齐上游（不传）、custom_message 线序 display/details 对调、parse_iso8601_ms 可选毫秒、read_text_lines 早停、bash 首帧 leading-edge、nodejs_env_test 卫生与断言收紧、proxy_test 断连噪音、prompt_templates 自引用断言收紧。
审查中两条不成立：read abort 文案（上游 nodejs.ts:121 即为 "aborted"，Rust 逐字一致）；Myers 中间蛇路线（经反例证明无法逐字节一致，改深度 checkpoint 分治）。明细见 D-020「审查修复轮」。
