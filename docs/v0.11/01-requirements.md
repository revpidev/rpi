# Pir v0.11 变更需求规格说明书（对齐 Pi v0.84.1）

> 本文档是 [`../01-requirements.md`](../01-requirements.md)（v0.1 基线）的**增量变更需求**，定义 pir 从 Pi v0.82.1 基线升级到 v0.84.1+ 基线所需的功能与行为变更。
> 未在本文档出现的 v0.1 需求继续有效。
> 对照源：`external/pi` @ `4181f66`（v0.84.1+，2026-08-08），旧基线 `2efa728`（v0.82.1）。跨度 461 commits / 655 文件（+59396/−15144 行）。
> 上游版本节奏：v0.82.1 → v0.83.0（2026-07-29）→ v0.84.0（2026-08-06，破坏性大版本）→ v0.84.1（2026-08-07）→ 少量 Unreleased。
> 术语 **[BREAKING]** = 上游标注的破坏性变更；**[PARITY]** = 直接影响行为对拍；**[DEFER]** = v0.11 明确不做，记录为已知缺口。

---

## 1. 升级总览与范围决策

### 1.1 变更分布

| 区域 | 规模 | 定性 |
|------|------|------|
| `packages/ai` | 67 commits，120 文件，+7138/−967 | 类型字段扩展 + 大量流式协议修复 + models refresh 事务化重构 |
| `packages/coding-agent` | 146 commits，215 文件，+11699/−1264 | JSON/RPC 线格式重构、fullscreen TUI、auth 命令族、远程 session 客户端栈 |
| `packages/agent` | 166 commits，79 文件，+14367/−8174 | harness v2 计划执行期：v1 harness 删除，v4 lane 会话存储层落地，v2 运行时仅 scaffold |
| `packages/tui` | 56 commits，49 文件，+7536/−827 | TUI 渲染架构重构（接口 + 双渲染器）+ 全屏子系统 + LaTeX |
| `packages/server` | 净重写，+3850/−1985 | 旧进程管理服务器（Radius/JSON-lines IPC）整体删除，重写为传输无关 PiServer |
| `packages/protocol` / `client` / `telemetry` | 三个新包，+6454 | 帧化 CBOR 线协议 / 传输无关客户端 / 遥测契约 |
| `packages/session-backends/sqlite-node` | 55 commits | 包更名（原 `storage/sqlite-node`）+ v4 lane schema 重写，旧库不迁移 |
| `packages/evals` | 27 commits | 扩展为 comparative eval 体系 |

### 1.2 v0.11 范围决策

延续 ADR-0003 的排除项并补充：

| 项 | 决策 | 理由 |
|----|------|------|
| `packages/server` 重写版 + `pi-protocol` + `pi-client` | **[DEFER]** | v0.1 已排除 server（ADR-0003）；上游 RemoteSession 尚未接入 CLI/TUI，仅是库导出。记录为缺口，待上游接入主流程后再评估 |
| 旧 server 协议（JSON-lines IPC/spawn/supervisor/Radius） | **删除**（如已复刻） | 上游 `05bf9df65` 已全删，无兼容期 |
| `session-backends/sqlite-node` | **[DEFER]** | v0.1 已排除 SQLite 存储；pir 仅 JSONL。v4 SQLite schema（writer lease、FTS5 trigram 等）暂不复刻 |
| `packages/telemetry` | **[DEFER]**（保留字段占位） | pir-ai 的请求选项类型保留 `telemetryContext` 等价字段；不实现遥测管线 |
| `packages/evals` comparative 体系 | **[DEFER]** | 评测基建非产品行为；其 session 快照归档格式可作对拍参考 |
| harness v2 运行时（prompt/steer/abort 等） | **[VARIANT]**：pir 保持 v1 语义 | 上游 v2 运行时未实现（仅 scaffold，操作全部 reject `HarnessNotImplemented`），且设计文档明确 D0 将重写 record 契约。**当前没有可对拍的上游运行时**，pir-agent 的 harness 层以 v0.1 已实现的 v1 语义为准，待上游 H0+ 落地后再对齐 |
| session v4 lane 存储契约 | **[DEFER]**（见 §4.3） | v4 只存在于 agent 包 harness 层；coding-agent 主路径 SessionManager JSONL 格式**未变**（v3，仅 3 行改动）。pir 主路径对拍不受影响 |

### 1.3 验收基线

- 对拍基准以 **v0.84.1（`4181f66`）为唯一基准**，不参考中间版本（TUI fullscreen 在周期内经历过一次整体回退重做）。
- 上游新增的对拍素材（可直接移植为 pir 测试期望）：`test/regressions/7290-json-stream-linear`、`mistral-http-transport.test.ts`、`openai-responses-terminal-event.test.ts`、`google-shared-signed-empty-blocks.test.ts`、`anthropic-sse-parsing`、`sampling-options.test.ts`、`fetch-option.test.ts`、`validation`（nullable union）、`tui-alt-screen.test.ts`（1067 行）、`latex.test.ts`（483 行）。

---

## 2. pir-ai 变更需求（↔ packages/ai）

### 2.1 消息与类型扩展 **[BREAKING][PARITY]**

- R2.1.1 `StopReason` 新增 `"deferred"`（v0.84.0 已有 `"pending"`）。序列化/协议层遇 `deferred` 必须显式处理（上游 server 协议 v1 选择**拒绝**：抛错而非静默吞掉）。
- R2.1.2 `AssistantMessage` 新增字段：`rawStopReason?: string`（覆盖 Anthropic/Google/Bedrock/Mistral/OpenAI completions/responses）、`endTurn?: boolean`（Codex 调试用途，不影响控制流）、`deferred?: DeferredHandle`（见 R2.2）。
- R2.1.3 `ToolCall` 新增 `namespace?: string`，streaming/proxy/replay 全程保留；proxy 的 `toolcall_end` 事件帧从 `{type, contentIndex}` 变为 `{type, contentIndex, toolCall}`（**影响 RPC/事件对拍**）。
- R2.1.4 `Model.samplingParams` + `StreamOptions.samplingParams`（`Record<string, unknown>`），请求体组装**最后**合并（键可覆盖命名参数），仅 OpenAI-compatible 适配器生效。
- R2.1.5 `OAuthAuth.isSubscription?: boolean` 元数据。

### 2.2 Deferred / background 请求（上游 v0.84.0 最大新能力，DRAFT 状态）

- R2.2.1 **[DEFER]**：`SimpleStreamOptions.deferred`、`fetchDeferred()/cancelDeferred()`、`DeferredHandle`（provider/modelId/api/id/expiresAt/pollAfterMs/data）、`wait` 长轮询语义、lazy 能力声明。上游尚处 DRAFT（#7339），仅 OpenAI background mode；pir v0.11 只落地 R2.1.1/R2.1.2 的类型字段与序列化兼容，不实现请求生命周期。
- R2.2.2 faux provider 对拍面：若 v0.11 实现对拍，需覆盖 pending/ready/failed/cancelled 四态（随 R2.2.1 一并 deferred）。

### 2.3 流终止语义修正 **[PARITY]（对拍差异高发区）**

- R2.3.1 未映射终端 reason 一律转为 **provider 错误**而非成功 stop：Anthropic `sensitive` → `stopReason: "error", errorMessage: "Provider stopped with: sensitive"`；Bedrock 未知 reason → `"Provider stopped with: <reason>"`；Mistral → `"Mistral stopped with: <reason>"`；OpenAI Responses 的 incomplete reason **只有 `max_output_tokens` 才是 length stop**，其余（`content_filter`/`max_time_limit` 等）→ `"Response incomplete: <reason>"`。
- R2.3.2 OpenAI completions：新 compat 标志 `supportsFinishReason`——provider 不发流式 `finish_reason` 时按内容推断 `toolUse`/`stop`；声明支持却缺失时报 `"Stream ended without finish_reason"`。
- R2.3.3 `aborted`/`error` 时错误消息携带 `output.errorMessage`（不再固定 "An unknown error occurred"）。
- R2.3.4 `isRecoverableLength()`：`stopReason === "length" && output < desiredMaxOutput` 判定可恢复截断（供 §3.4 的 compact-and-retry 使用）。

### 2.4 流式协议细节修复 **[PARITY]**（逐项对齐）

- R2.4.1 Anthropic：`content_block_start` 的 text/thinking **初始内容不再丢弃**（`59ad3dead`）。
- R2.4.2 Google：带 thought signature 的空 text/thinking block 必须保留；`requiresToolCallId()` 扩展到 Gemini 3.x+（`cbaca6038`）；GenAI SDK 错误（408/409/429/5xx + retry-after）纳入统一重试（`retryGoogleRequest()`）。
- R2.4.3 Mistral：SDK 移除，改原生 SSE 传输（自解析 `data:`/`[DONE]`/多行 JSON）；请求字段 camelCase→snake_case 映射（`toMistralWirePayload()`）；`x-affinity` 头保留。
- R2.4.4 Codex：WebSocket session 缓存键从 `sessionId` 改为 `sessionId → accountId` 二级 Map（不同账号不共享连接）。
- R2.4.5 tool-call delta 解析：同一 delta 含合法 `function` 与空 `custom` 时不再丢弃 function 参数。
- R2.4.6 工具参数 union 校验：先做"原值已匹配任一分支"检查，避免 nullable union 把 `null` 强转（`2e95584da`）。
- R2.4.7 错误体规范化：仅 plain object 视作结构化响应体（`4523528b2`）；新增可重试文案 `"exceeded request buffer limit while retrying upstream"`。
- R2.4.8 OpenAI-completions `useMaxTokens` 名单新增 **DeepSeek**、**Z.AI**（这些端点用 `max_tokens` 而非 `max_completion_tokens`）；qwen thinking 分支支持 `reasoning_effort` 映射。
- R2.4.9 vLLM `supportsThinkingTokenBudget`（opt-in）：按思考档位预算（minimal 1024/low 2048/medium 8192/high 16384，可覆盖），**始终预留 `MIN_ANSWER_TOKENS = 1024`** 给最终回答。
- R2.4.10 Responses `additional_tools` 延迟工具模式：`deferredToolsMode: "additional-tools" | "tool-search"`，GPT-5.6 系优先消息锚定的 `additional_tools` 输入项；namespace 回放规则（同模型或已加载 deferred 工具才回放）。
- R2.4.11 llama.cpp：`supportsUsageInStreaming: true`（流式不再零 usage）。

### 2.5 Models refresh 事务化重构 **[BREAKING][PARITY]**

- R2.5.1 两阶段 refresh：先无条件 restore（`allowNetwork=false` 也 restore，且在 auth 解析**之前**），再按需 fetch。
- R2.5.2 generation 守卫：`setProvider/deleteProvider/clearProviders/refresh` 均 supersede 上一代；发布必须过 `publishProviderModels()` 的 generation 检查；按 provider 串行发布；`structuredClone` 快照写入。
- R2.5.3 `RefreshModelsContext` 重写：删 `context.store`，改为只读 `context.stored` + `context.publish({persist?, update?})`（`persist: null` 删除，省略 = 不持久化）；`signal` 必选；调用方给 signal 时 `refresh()` 可返回 `{aborted: true}`；`ModelsRefreshOptions.providers` 支持定向刷新；错误按 provider 收集到 `errors` map。
- R2.5.4 `ModelsStreamTransforms` → `ModelsRequestTransforms`（header 变换作用于所有认证请求）。

### 2.6 OAuth / 凭证行为 **[PARITY]**

- R2.6.1 提前刷新窗口：剩余有效期 < 5 分钟即锁内刷新（原到期才刷）；`minOAuthValidityMs` 显式要求时刷新后仍不足抛 `ModelsError("oauth")`。
- R2.6.2 刷新加 **15 秒硬超时**（`AbortSignal.any([caller, timeout(15s)])`），防卡死持锁。
- R2.6.3 所有 auth 操作强制接受 signal；`InMemoryCredentialStore` 队列等待中被 abort 立即拒绝且不阻塞后续。

### 2.7 新 provider 与目录

- R2.7.1 **Baseten**：OpenAI-compatible，`BASETEN_API_KEY`，`https://inference.baseten.co/v1`；`thinkingFormat: "baseten"`（toggle）/ `"openai"`（effort）按能力自动选择；compat 新增 `chatTemplateArgs`；跳过 `status === "deprecated"` 模型。
- R2.7.2 **Qwen Token Plan Individual**：白名单 7 模型，共享 `QWEN_TOKEN_PLAN_API_KEY` 与国际端点；生成脚本侧 `assertExactModelIds()` 严格对拍。
- R2.7.3 目录修正：`qwen3.8-max-preview` → `qwen3.8-max`；Copilot Individual 端点 picker 全 false 时回退 `policy.state === "enabled"` 列表（`parseAvailableCopilotModelIds`）；Copilot Grok 4.5 改走 Responses API；Fireworks Kimi K3 改 OpenAI-compatible + reasoning-effort + deferred tools；GLM 5.2 不发 `prompt_cache_retention` 并启用 session affinity；GPT-5.6 Terra/Luna 价格覆盖。
- R2.7.4 每请求 `fetch` 注入：`ProviderRequestOptions.fetch` 贯通各 SDK client；Google 适配器拒绝非 global fetch。

### 2.8 类型结构（非运行时）

- R2.8.1 `StreamOptions` 拆分为 `ProviderRequestOptions`（signal/telemetryContext/apiKey/fetch/env/onPayload/onResponse/headers/timeoutMs/maxRetries/maxRetryDelayMs）+ `StreamOptions`；pir 对应 Rust 类型做同等分层。

---

## 3. pir 主路径变更需求（↔ packages/coding-agent）

### 3.1 JSON/RPC `message_update` 线格式重构 **[BREAKING][PARITY]（最高优先级）**

- R3.1.1 `message_update` 事件**移除**累积 `message` 字段与 `assistantMessageEvent.partial`，只发增量 delta（`contentIndex` + `delta`）；消费方在 `message_start`/`message_end` 间自行拼装，`message_end.message` 为权威终态（`a4475344f`，修复 #7290 二次方输出）。
- R3.1.2 实现对应 `toJsonEvent()`（print-mode 与 rpc-mode 共用同一转换）；`RpcClient` 事件类型同步为 delta 形态。
- R3.1.3 **stdout backpressure**：print/rpc 模式写出前等待 `waitForRawStdoutBackpressure()` 等价物，防止大输出拖垮管道。
- R3.1.4 pir 的 RPC 文档与 fixtures 同步更新；`start`/`done`/`error` delta 类型从 RPC 文档表删除。

### 3.2 UI 模式：fullscreen TUI **[PARITY]（周期最大工程）**

- R3.2.1 CLI 参数 `--ui-mode regular|fullscreen`（`--alt` 保留兼容映射，帮助文本移除）；后更名 `--tui-mode`，settings 键 `uiMode` → **`tuiMode`** **[BREAKING：旧配置键被忽略回退默认]**。
- R3.2.2 运行时经 `/settings` 热切换渲染器（保留组件树重挂载 + 渲染状态 capture/restore）。
- R3.2.3 配套设置：`fullscreenExitOutput`（`transcript`|`resume-hint`，退出打印完整 transcript 或仅 resume 提示）、`fullscreenScrollbar`（`auto`|`always`|`hidden`）、`scrollbarThumb` 主题色。
- R3.2.4 全屏交互：sticky editor/footer dock、独立滚动 transcript、可拖拽滚动条、PageUp/Down（4 行重叠）与 Home/End 导航、OSC 133 prompt 跳转（`ctrl+shift+up/down`）、半页滚动 action、双/三击选择、堆叠式 flash 通知。
- R3.2.5 子进程环境新增 **`AI_AGENT=pi`**（pir 侧为 `AI_AGENT=pir`，按 APP_NAME 派生惯例）。

### 3.3 渲染与内容

- R3.3.1 **Mermaid 渲染**：`markdown.mermaid` 设置（`off`|`final`|`streaming`，默认 streaming）。上游依赖 grok-mermaid（TS），其本身是 [xai-org/grok-build](https://github.com/xai-org/grok-build)（Apache-2.0）`xai-grok-markdown/src/mermaid.rs` 的移植——pir 直接移植该 Rust 原作（设计 §5.6），无 [VARIANT] 缺口。
- R3.3.2 **LaTeX 渲染**：继承 pir-tui 的 Unicode math（见 §5.4）。
- R3.3.3 扩展 API `pi.registerMarkdownTransformer()`：链式、宽度感知（context 含 `messageType`/`isStreaming`/`availableWidth`），作用于 assistant/user/thinking 渲染。

### 3.4 Agent 会话行为 **[PARITY]**

- R3.4.1 **length-stop 恢复**：`stopReason === "length"` 且输出低于 outputLimit → 自动 compaction + **重试一次**；`_overflowRecoveryAttempted` 在 length stop 后不再重置；TUI 截断提示改为中性文案（`32850ef7c`）。
- R3.4.2 **compaction 期间 prompt 拒绝**：进行中 `prompt()` 直接抛错（原静默丢失）；`compaction_end` 事件发出**前**清 abort controller，使扩展可提交 queued prompts；修复手动/自动 compaction 竞态（`8eda4f5b2`/`3852cb2b8`/`e56893f4c`，三 commit 相互依赖需一并实现）。
- R3.4.3 **settings 递归深合并**：`deepMergeSettings` 从浅层展开改为递归合并（修复 #7572：项目级 `retry.provider` 局部设置不再抹掉全局其他字段）。
- R3.4.4 **工具结果图片统一规范化**：所有工具（含扩展注入/替换）返回的 image block 进 history 前过 `normalizeToolResultImages()`（挂 `afterToolCall`，在扩展 `tool_result` hook 之后）；`images.autoResize` 可关；失败保留原图。
- R3.4.5 **`--model` 精确 ID 歧义**：多 provider 命中时优先唯一已认证 provider，0 或 >1 个认证则报歧义错误（原取目录首个）；`/model`、`/scoped-models` 改走缓存快照即时渲染。
- R3.4.6 **ModelRuntime 可用性刷新代际序列化**：`availabilityRefreshSeq` 等代际计数防 stale 发布；强制刷新不被 stalled 刷新阻塞；`/model` 失败列出每个失败 catalog。
- R3.4.7 **凭证操作串行化**：login/logout/setRuntimeApiKey/removeRuntimeApiKey 经 `credentialOperations` map 串行；新增 `CredentialSynchronizationError`；读取前重载凭证；消除文件锁 convoy。
- R3.4.8 **认证请求保留 credential-resolved `baseUrl`**：扩展模型调用（自定义 compaction、handoff、Q&A）不再丢按凭证解析的端点。
- R3.4.9 **session 替换/树导航先 abort 持久化**：`teardownCurrent()` 先 `await session.abort()` 再发 `session_shutdown`（#7022）；session 发现支持符号链接目录。
- R3.4.10 扩展运行时事件总线泄漏修复：`invalidate()` 统一退订。
- R3.4.11 bash 工具 `PI_*` 提示软化（"You can inspect ..."，#7128）；find 路径相对化重写（`relativizeFindResultPath()`，trailing separator 保留，Windows fd 模式 `[/\\]`）。
- R3.4.12 管理 HTTP `fetchWithRetry()`（408/425/429/500/502/503/504 + 总超时预算），仅用于 version-check/catalog/managed-tool/package 下载。

### 3.5 资源与上下文

- R3.5.1 **`AGENTS.override.md`**：候选顺序 `AGENTS.override.md` > `AGENTS.md` > `AGENTS.MD` > `CLAUDE.md` > `CLAUDE.MD`。
- R3.5.2 扩展 reload 资源后保留 skills/prompts/themes 的 package source 元数据（#6968）；嵌套 worktree 上下文文件去重（`findShadowedContextFile()` 用 commonGitDir/mainRepoRoot）。
- R3.5.3 `ResourceLoader` 接口新增 `getSystemPromptSource`/`getAppendSystemPromptSources`（继承 v0.83.0）。

### 3.6 auth 命令族

- R3.6.1 `pi auth print-api-key` / `print-bearer-token`：导出凭证给外部客户端，自动 OAuth 刷新 + `--min-expiry <duration>`（默认 5 分钟最小有效期）。
- R3.6.2 `pi auth check`：provider/model 认证预检，`--json`/`--credentials`/`--no-refresh`，退出码 ready=0 / not_ready=1 / invalid=2。

### 3.7 包管理

- R3.7.1 git 包安装容错：`git clean` 失败后检测缺失依赖重装；安装失败清理残留；`.pi-update-incomplete` marker 续传（pir 侧 `.pir-update-incomplete`）。
- R3.7.2 `readPiManifest()`：package.json `pi` 字段解析独立化 + 类型校验。

### 3.8 远程 session 客户端栈 **[DEFER]**

- R3.8.1 `src/client/`（RemoteSession 状态机 unbound/ready/busy/disposed + transcript reducer）与实验性 CLI 组合框架（`src/cli/experimental/`）整体 defer（见 §1.2）。记录：上游亦未接入 CLI/TUI。

---

## 4. pir-agent 变更需求（↔ packages/agent）

### 4.1 Agent 循环微行为 **[PARITY]（对拍面小，全部需做）**

- R4.1.1 `AgentOptions.shouldStopAfterTurn`：turn 结束后、轮询队列前优雅停止；回调第二参数带 `AbortSignal`。
- R4.1.2 阻塞工具调用 `terminate`：扩展 `tool_call` handler 返回 `{block: true, terminate: true}`，整批工具结果都 terminate 时跳过后续模型调用直接结束回合。
- R4.1.3 `Agent.reset()` 在活跃 run 期间抛错（原静默清状态）。
- R4.1.4 proxy `toolcall_end` 帧携带完整 `toolCall` 对象（同 R2.1.3）；`samplingParams` 透传。

### 4.2 Compaction 契约收紧 **[BREAKING]**

- R4.2.1 `CompactionResult` → `CompactResult`；删 `firstKeptEntryId`；`retainedTail` 改必填；`extractFileOperations` 不再检查 `fromHook`；cut-point 只认 `branch_summary`。

### 4.3 会话存储 v4（lane-based） **[DEFER，部分跟踪]**

- R4.3.1 上游 v4（header `{kind:"header",version:4,...}` + 共享 seq 的 `entry|record|lane|fact` 行、7 种 Entry、9 种 LaneRecord、原子发布、torn-tail 截断恢复、id 按 cwd 作用域、`FileSystem.renameFile()` 必选）**仅属于 agent 包 harness 层**；coding-agent 主路径 SessionManager 格式未变。
- R4.3.2 pir v0.11 决策：主路径 SessionManager 继续对拍 v3（无变化）；pir-agent harness 存储**保持 v1 语义**（上游 v2 运行时未落地，record 契约还将被 D0 重写，现在复刻等于对拍一个过渡态）。**例外**：`FileSystem` 等价 trait 预留 `rename_file()`（原子发布是确定性收益）。
- R4.3.3 若未来跟进 v4：三后端一致性套件 `session/testing/conformance.ts`（1016 行）是对拍蓝本；注意上游 v4 repo **读不了 v3 文件**（J4/J5 归一化未实现），pir 若需读旧会话须自建迁移。

### 4.4 Harness v2 事件/遥测 **[DEFER]**

- R4.4.1 `HarnessEventBus`（`run_start`/`run_end` + watch 缓冲）已落地但未从包入口导出（上游 I2 工作包保留中）；pir 待上游完成再对齐。
- R4.4.2 遥测 schema（`AI_TELEMETRY_SCHEMA` / `HARNESS_TELEMETRY_SCHEMA` 11 种 span）是稳定输入，但运行时插桩未落地；随 telemetry 整体 defer。

---

## 5. pir-tui 变更需求（↔ packages/tui）

### 5.1 渲染架构重构 **[BREAKING]**

- R5.1.1 `TUI` 从具体类变为**接口 + 抽象基类 + 双渲染器**：`TuiBase`（输入分发、overlay 栈、渲染调度、颜色查询）+ `TuiMainScreen`（旧差分渲染，逐行等价已验证）+ `TuiAltScreen`（全屏渲染）。pir 对应 trait 化。
- R5.1.2 `stop(options)` 参数化：`preserveScreen: true` 时 main-screen 不写光标归位序列、alt-screen 直接退出不重打文档。
- R5.1.3 `captureRenderState()/restoreRenderState()`（main-screen 7 个渲染状态字段）支持无重放模式切换；`ViewportTUI`/`setLayoutRoot`。
- R5.1.4 输入监听器类型更名 `InputListener` → `TuiInputListener`；`compositeTuiLine` 提升为公共函数。

### 5.2 全屏渲染器（全新子系统）

- R5.2.1 终端控制：`\x1b[?1049h/l` 进出、禁用自动换行、SGR 1006 + 1002/1003 鼠标跟踪（tmux/Zellij/Screen 下用 button-motion 1002）、focus in/out、同步输出包裹。
- R5.2.2 布局引擎：`VStack/HStack`（basis/grow/shrink/min/max/gap 约束求解）、`ScrollView`（follow、overscroll 链式 `chain|contain`、滚动条三态、transient 1s 隐藏、scrollBy 返回未消费增量）、clip 传播、按宽度 render cache、`LAYOUT_NODE` 协议（Rust 以 trait 替代 symbol）。
- R5.2.3 鼠标交互：滚轮（默认步长 1 行）、点击、双/三击选词选行（`DOUBLE_CLICK_INTERVAL_MS=500`）、grapheme 边界吸附、边缘自动滚动、滚动条拖拽、OSC 8 URL 点击、win32 右键粘贴（bracketed paste 注入）。
- R5.2.4 剪贴板：OSC 52 复制 + flash 确认。
- R5.2.5 Kitty 图片：全局元数据注册表（LRU 1000）、placement-only 重发、像素级裁剪、离屏缓存（16 张/32MB 传输/64MB 解码上限）与淘汰删除；iTerm2 payload 补 `size=`。
- R5.2.6 退出语义：文档逐行 `\r\x1b[2K` 重打主屏（剥 OSC 133 前缀、恢复自动换行），或 `preserveScreen` 直接退出。

### 5.3 既有行为修正 **[PARITY]**

- R5.3.1 **键盘输入不再受 16ms 节流**：输入转发后走立即渲染路径（渲染帧时序变化，逐帧对拍基线需重录）。
- R5.3.2 **grapheme 宽度算法更新**（#6987）：`Spacing_Mark` 减 `\u1734 \u302E \u302F` 例外 + 12 个非间距例外字符；连字簇中 mark 后 Indic 辅音、半/全宽 forms（0xFF00-0xFFEF）、泰/老挝 AM 元音逐个 +1；无基字符 spacing mark 按码点数计宽。**pir 的 wcwidth 逻辑须精确对齐例外表**（影响 truncate/换行/布局/光标列）。
- R5.3.3 `truncateToWidth` 截断时若前缀处于活跃超链接，省略号前插入 OSC 8 关闭序列（#7657）；纯文本前缀跳过 OSC 8 扫描。
- R5.3.4 颜色方案报告批量解析：`^(?:\x1b\[\?997;(1|2)n)+$`。
- R5.3.5 终端进度清除序列修正：`\x1b]9;4;0;\x07` → `\x1b]9;4;0\x07`。
- R5.3.6 SettingsList 搜索空格参与多词过滤（不再剥离）。
- R5.3.7 Editor 默认键位扩充：`cursorLineStart` = `[home, ctrl+home, ctrl+a]`，`cursorLineEnd` 同理，`pageUp/pageDown` 加 `ctrl+pageUp/ctrl+pageDown`；新增可配置 action `tui.editor.historyPrevious/Next`（默认无键）。
- R5.3.8 keybindings 新增 `tui.altScreen.*` 8 个 action；全屏模式下裸 `pageUp/pageDown/home/end` 被视口消费、不再到达编辑器。
- R5.3.9 Windows：Shift+Enter 检测（native helper 修饰键查询）、truecolor 检测放宽（无 `WT_SESSION` 也启用）。

### 5.4 Markdown / LaTeX

- R5.4.1 LaTeX→Unicode 渲染器：`$...$`/`$$...$$`/`\(...\)`/`\[...\]` tokenizer（含转义与 pending 未闭合处理），分数/上下标/根式/矩阵/对齐/cases/运算符 limits/符号表/间距命令；`MarkdownOptions.renderLatex`（默认 true）；`renderLatex()` 公共 API。
- R5.4.2 Markdown 组件 `transform?: (md, availableWidth) => string` 宽度感知源变换（服务 §3.3.3 扩展 API）。

### 5.5 性能

- R5.5.1 整行 box 直接引用源行渲染（分配减少 9–18x，`18dee5f0a`）；以 `render-churn-bench.ts` 场景为 pir 基准参考。

---

## 6. 扩展 API 面变更汇总 **[BREAKING]（pir-ext-sdk / pir-ext-host 同步）**

| 变更 | 说明 |
|------|------|
| `refreshModels` context 重构 | `context.store` 删除 → 只读 `context.stored` + `context.publish({persist?, update?})`；手写 `refreshModels` 的 provider 必须迁移 |
| OAuth `refreshToken(credentials, signal)` | signal 必选；新增 `isSubscription` 可选字段 |
| `ModelRegistry.refresh()` | 接受 `ModelsRefreshOptions`，返回 `ModelsRefreshResult`（per-provider errors + aborted） |
| `ModelRegistry.setRuntimeApiKey()` | 变 async；参数改为 auth 取消选项 |
| `ModelRegistry.getApiKeyAndHeaders()` | 返回 `ProviderHeaders`（`string \| null`），保留 null 删除标记，转发必须原样传递（#7030） |
| `ctx.scopedModels` / `getScopedModels` | 扩展上下文暴露解析后的会话模型范围（只读快照） |
| `tool_call` handler `terminate` | 见 R4.1.2 |
| `registerMarkdownTransformer()` | 见 R3.3.3 |
| `modelRegistry.complete/find/hasConfiguredAuth` | 扩展内 LLM 调用统一入口（替代手动 auth + compat complete） |
| `ResourceLoader` 新增方法 | 见 R3.5.3 |
| 工具 system prompt 贡献外露 | bash/find/edit/read/write/grep/ls 各导出 `xxxToolSystemPromptContribution` 常量 |
| TUI 类型面 | `new TUI()` → `TuiMainScreen`；`TuiMode`/`TuiStopOptions`/`ViewportTUI` |
| TypeBox 1.1.38 → 1.3.7 | 移除 `Type.Base/Awaited/Promise/AsyncIterator/Iterator/Options`、`Value.Mutate`；修复 nullable 数组校验（pir 的 Wasm 扩展参数校验逻辑同步） |

---

## 7. 验收标准（增量）

在 v0.1 §11 验收框架上追加：

1. **JSON/RPC 对拍**：`message_update` delta-only 输出与上游逐字节一致（允许时间戳/ID 差异）；大输出场景验证 backpressure；`7290-json-stream-linear` 回归移植通过。
2. **流终止对拍**：R2.3/R2.4 每条修复至少一个 golden 用例（rawStopReason 覆盖 6 个 provider 家族）。
3. **settings 合并对拍**：#7572 场景（项目级局部 `retry.provider` + 全局其他字段）递归合并结果一致。
4. **length-stop 恢复链**：可恢复截断 → compact + 单次重试的事件序对拍；compaction 期间 prompt 拒绝 + queued prompt 在 `compaction_end` 前可提交。
5. **TUI 对拍**：main-screen 渲染字节流基线重录（渲染时序变化）后回归；fullscreen 子系统以 `tui-alt-screen.test.ts` 30+ 场景为蓝本验收；宽度算法例外表以 `latex.test.ts`/`utils` 测试对齐。
6. **--model 歧义**：多 provider 命中三态（唯一认证/零认证/多认证）行为对拍。
7. **auth 命令**：`auth check` 退出码 0/1/2、`print-api-key --min-expiry` 刷新阈值对拍。
8. **扩展 API**：§6 表格每项有编译期或运行时校验；`getApiKeyAndHeaders` null 透传有防泄漏用例。

---

## 8. 风险与依赖

- **上游契约仍在演化**：harness v2 的 D0（record 契约重写）、J4–J6（v3 归一化）、H0+（运行时）、I2（事件导出）均未落地。v0.11 边界选择"存储层不动 + Agent 循环微行为 + 产品层全量"，harness 运行时明确保持 v1 语义（§1.2）。
- **变更簇相互依赖**：length-stop 恢复链（`32850ef7c`+`e56893f4c`+`8eda4f5b2`+`3852cb2b8`）须作为整体实现，单独摘取会引入新竞态。
- **渲染时序基线重录**：R5.3.1 使所有逐帧 TUI 黄金文件失效，需在升级早期统一重录。
- **Mermaid（R3.3.1）移植适配**：grok-mermaid 的 Rust 原作（xai-org/grok-build `mermaid.rs`）依赖 ratatui 样式类型，移植时需做样式模型映射层；以 grok-mermaid fixtures 双向校验防移植偏差。
