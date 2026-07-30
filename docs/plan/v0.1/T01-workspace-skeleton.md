# T01：工程骨架与类型契约锁定

- **状态**：已完成
- **里程碑**：M0
- **依赖**：—
- **上游对照**：`external/pi/packages/{ai,agent,tui,coding-agent}` 包结构与类型定义
- **需求章节**：§1.4、§4.1、§4.2（类型面）；§6.2（session 条目类型）；§10（单文件部署）
- **预估**：0.5–0.7 人月（M0 共 1–1.5，与 T02 合计）

---

## 目标

建立可编译的 Rust workspace 骨架，锁定跨 crate 的核心类型与事件枚举契约，
使后续所有任务在稳定的地基上并行开发。

## 范围

### In

- workspace `Cargo.toml` + `rustfmt.toml` + release profile（编码规范 §15.4）
- 六个 crate 骨架：`pir-ai`、`pir-agent`、`pir-tui`、`pir`（bin + lib）、`pir-ext-host`、`pir-test-support`，依赖方向按设计文档 §2.2
- `pir-ai` 核心类型：`Role` / `Message` / `Context` / `Tool`（含 `constrained_sampling`）/ `Model`（含 `thinkingLevelMap` 三态、cost.tiers、headers、compat）/ `ApiKind` / `AssistantMessage`（含 `stopReason` 全集 `stop|length|toolUse|error|aborted`、`pending` 仅瞬时不入 JSONL；`api/provider/model/responseModel?/responseId?/diagnostics?/usage/errorMessage/timestamp`）
- 签名与诊断字段：`ToolCall.thought_signature?`、`TextContent.text_signature?`、`ThinkingContent.thinking_signature?/redacted?`、`Usage.cache_write1h?/reasoning?`
- `pir-ai` `StreamEvent` 枚举完整定义（M0 锁定，见编码规范 §4.1）：**变体携带 `content_index`，不同 block 事件可交错**
- `pir-agent` `AgentEvent`（10 种，**含载荷**：`turn_end{message, toolResults}`、`agent_end{messages}`、`message_update{assistantMessageEvent}` 等）/ `AgentTool`（含 `execution_mode`、`prepare_arguments`）/ `AgentMessage` 联合类型（含 `bashExecution` / `custom` / `branchSummary` / `compactionSummary` 全字段，`ToolResultMessage.details/usage/addedToolNames/isError`）
- 扩展消息 → LLM 文本格式常量：`COMPACTION_SUMMARY_PREFIX/SUFFIX`、`BRANCH_SUMMARY_PREFIX/SUFFIX`（逐字移植）
- session 条目类型 serde 骨架：header + 9 种主路径条目 + compaction 两形态（`firstKeptEntryId` / `retainedTail`）+ harness 独有 `active_tools_change` / `leaf`（需求 §6.2）
- `StreamFn` 类型别名与 `BoxStream` 定义（设计文档 §4.4）
- 各 crate 主错误枚举占位（`AiError` / `AgentError` / …，`thiserror`）
- 上游 pin 校验脚本（比对 `external/pi` HEAD 与 `UPSTREAM.md`）
- 模块风格：无 `mod.rs`（编码规范 §3.1）

### Out

- 任何 API 适配器实现（T03）、agent loop 逻辑（T05）、harness 层（T16）、TUI 渲染（T11）
- 对拍 harness（T02）

## 开发要点

- 类型字段命名镜像上游 TS 定义；线格式相关的 serde 属性本任务可先落 camelCase 骨架（编码规范 §4.4）
- 事件枚举变体顺序、字段命名与上游逐项核对后锁定；锁定后变更必须过门禁 G3 并更新 fixtures
- 每个 crate `lib.rs` 写模块文档，标注对应上游包（编码规范 §14.3）
- pin 校验脚本建议 `scripts/verify-upstream.sh`，输出 commit 并与 `UPSTREAM.md` 期望值比对

## 进度跟踪

- [x] 设计细化
- [x] 实现
- [x] 自测
- [x] 门禁验收
- [x] 文档回写

## 自测清单

- [x] `cargo build --workspace` / `clippy -D warnings` / `fmt --check` 通过
- [x] 类型单测：`StreamEvent` / `AgentEvent` / session 条目序列化形状快照测试（含 compaction 两形态、签名字段）——30 例全过
- [x] pin 校验脚本在正确 commit 上通过；人为切到错误 commit 时失败（验证脚本有效性后切回）
- [x] 依赖方向检查：`pir-agent` 不依赖 provider 实现、`pir-tui` 不依赖 `pir-ai`/`pir-agent`（已用 `cargo tree` 核对，内部边：pir → {pir-ai, pir-agent, pir-tui, pir-ext-host}，pir-agent → {pir-ai}，其余无内部依赖）

## 门禁验收

通用门禁 G1–G7 全过（G3 本任务以「类型序列化快照测试」替代 fixtures 对拍，验收记录中说明）。

任务特有标准：

- [x] 六个 crate 均编译通过且依赖方向与设计文档 §2.2 一致
- [x] `StreamEvent` / `AgentEvent` 与上游 TS 定义逐项核对清单完成（附在验收记录）
- [x] release profile 已配置且 `cargo build --release` 通过
- [x] pin 校验脚本落地并验证有效

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| D-001 | session 条目类型单一来源化（`pir-agent::session`，合并 coding-agent 与 harness 两套定义） | 已回写 |
| D-002 | TS 类型系统特性的 Rust 表达（声明合并折叠、compat 条件类型合并、AgentTool trait 化、Api 开放联合 newtype 化） | 已回写 |

## 验收记录

- 验收日期：2026-07-30
- 验收人：单人开发，按 gates.md §1 逐项自证
- G1 构建/静态检查：通过。`cargo build --workspace` ✅；`cargo clippy --workspace --all-targets -- -D warnings` ✅（0 warning）；`cargo fmt --all -- --check` ✅
- G2 测试：通过（30 passed, 0 failed；无 live 测试）
- G3 对拍：以「类型序列化快照测试」替代（本任务无运行行为，类型即契约）。快照测试 30 例锁定：`StreamEvent` 12 变体 type 标签 + contentIndex 驼峰 + toolcall_end 内嵌带 `type:"toolCall"` 标签的 ToolCall；`AgentEvent` 10 变体载荷（toolCallId/toolResults/assistantMessageEvent 驼峰）；session header + 11 条目（含 compaction `firstKeptEntryId` / `retainedTail` 两形态、`parentId`/`targetId` 显式 null、`| undefined` 字段缺省省略）；签名字段（thoughtSignature/textSignature/thinkingSignature/redacted/cacheWrite1h/reasoning）；`Model` 三态 thinkingLevelMap + cost.tiers + headers + compat（含 supportsOpenAIGrammarTools 首字母大写更名、chat_template_kwargs `$var`、OpenRouterRouting snake_case）；`COMPACTION_SUMMARY_PREFIX/SUFFIX`、`BRANCH_SUMMARY_PREFIX/SUFFIX` 字节级字面值
- G4 红线：通过。`external/pi` 无改动且 HEAD=2efa728（verify-upstream.sh 双重校验）；无 JS/TS 执行能力；未读写 `~/.pi`；无 SQLite；无 token 估算代码（本任务不涉及）；非测试代码无 `unwrap()`/`expect()`（脚本化核查：全部 panic 调用位于 `#[cfg(test)]` 模块内）；无凭据进日志/Debug（`StreamOptions` 手写 Debug 脱敏 api_key 与 header 值）；未引入范围排除项；无 rg/fd 下载机制；无 session 文件锁
- G5 线格式：通过（G3 快照测试合并执行；camelCase 经 `rename_all` / 逐字段 `rename` 与上游逐项核对）
- G6 文档同步：通过。移植文件头部均有溯源注释（上游路径 + 0.82.1/2efa728 + 有意差异说明）；偏离回写至 `02-design.md` §3.2/§4.1/§12；设计文档 crate 划分无变化
- G7 偏离闭环：通过。D-001、D-002 已登记（deviations/README.md 登记表）并回写，状态 `已回写`；均为实现细节偏离（线格式兼容，快照测试兜底），不需要 ADR
- 结论：通过

### 附：`StreamEvent` / `AgentEvent` 与上游 TS 逐项核对清单

`StreamEvent`（上游 `AssistantMessageEvent`，`packages/ai/src/types.ts`）→ `crates/pir-ai/src/types.rs`：

| 上游变体 | Pir 变体 | 载荷核对 |
|----------|----------|----------|
| `start{partial}` | `Start{partial}` | ✅ partial 含 `role:"assistant"` 标签 |
| `text_start{contentIndex,partial}` | `TextStart{content_index,partial}` | ✅ contentIndex 驼峰 |
| `text_delta{contentIndex,delta,partial}` | `TextDelta{...}` | ✅ |
| `text_end{contentIndex,content,partial}` | `TextEnd{...}` | ✅ |
| `thinking_start/delta/end` | `ThinkingStart/Delta/End` | ✅ 同 text 三件套 |
| `toolcall_start{contentIndex,partial}` | `ToolCallStart`（rename `toolcall_start`） | ✅ 非 `tool_call_start` |
| `toolcall_delta{contentIndex,delta,partial}` | `ToolCallDelta` | ✅ |
| `toolcall_end{contentIndex,toolCall,partial}` | `ToolCallEnd` | ✅ toolCall 独立序列化自带 `type:"toolCall"`（`tagged_tool_call` with 模块） |
| `done{reason:stop\|length\|toolUse,message}` | `Done{reason:DoneReason,message}` | ✅ reason 收窄为独立枚举 |
| `error{reason:aborted\|error,error}` | `Error{reason:ErrorReason,error}` | ✅ 同上 |

`AgentEvent`（`packages/agent/src/types.ts`）→ `crates/pir-agent/src/types.rs`：

| 上游变体 | Pir 变体 | 载荷核对 |
|----------|----------|----------|
| `agent_start` | `AgentStart` | ✅ 无载荷 |
| `agent_end{messages}` | `AgentEnd{messages: Vec<AgentMessage>}` | ✅ |
| `turn_start` | `TurnStart` | ✅ |
| `turn_end{message,toolResults}` | `TurnEnd{message,tool_results}` | ✅ toolResults 驼峰 |
| `message_start{message}` | `MessageStart{message}` | ✅ |
| `message_update{message,assistantMessageEvent}` | `MessageUpdate{message,assistant_message_event: Box<StreamEvent>}` | ✅（Box 仅内存布局，serde 形状不变） |
| `message_end{message}` | `MessageEnd{message}` | ✅ |
| `tool_execution_start{toolCallId,toolName,args}` | `ToolExecutionStart{...}` | ✅ args=`Value`（上游 `any`） |
| `tool_execution_update{...,partialResult}` | `ToolExecutionUpdate{...,partial_result}` | ✅ |
| `tool_execution_end{...,result,isError}` | `ToolExecutionEnd{...,result,is_error}` | ✅ |
