# Pir 需求规格说明书（1:1 对齐 Pi v0.82.1）

> 本文档定义 `pir` 必须达到的功能与行为。除非标注 **[DEFER]** 或 **[VARIANT]**，均要求与 Pi 行为一致。
> 对照源：`external/pi/packages/{ai,agent,tui,coding-agent}` @ `2efa728`（v0.82.1）
> 范围决策：[ADR-0003](./adr/0003-coverage-review-scope-decisions.md)

---

## 1. 产品目标

### 1.1 愿景

用 Rust 实现与 Pi 同构的 **minimal terminal coding agent harness**：默认精简工具集，通过扩展/技能/模板/主题/包进行扩展，支持交互、脚本、RPC 与库嵌入。

### 1.2 成功标准

1. **行为对拍**：同一配置下，print/json/rpc 模式对同一 prompt 的工具调用序列、session JSONL 结构、事件类型与 Pi 一致（允许时间戳/随机 ID 差异）。
2. **会话互通**：能加载并续跑 Pi 生成的 session JSONL（v1–v3 自动迁移仅限主路径 SessionManager；agent harness 的 JsonlSessionStorage 硬性要求 v3，见 §6.1）。
3. **资源互通**：Skills / Prompt Templates / Themes / Settings / Keybindings / `models.json` 文件格式兼容；Auth 凭据存储（credential store）JSON 结构与 Pi 兼容（仅路径在 `~/.pir` 下，可手动拷贝迁移登录态）。
4. **架构同构**：四层 crate 边界与 Pi 四包对应。
5. **扩展**：ExtensionAPI **形状同构**，实现为 **Rust / Wasm**（见 [ADR-0001](./adr/0001-extension-and-config-dir.md)）。**不要求**兼容现有 TypeScript / jiti pi-package 扩展。

### 1.3 非目标（与 Pi 一致）

- 不内置子 agent / plan mode / MCP / todo / 后台 bash / 权限弹窗（由扩展提供）
- 不内置细粒度 OS 权限沙箱（文档化 containerization 模式即可）
- 不以「更好的 UX 创新」替代 1:1（创新放后续版本）

### 1.4 命名与路径（已决策）

| 项 | Pi | Pir（默认） |
|----|-----|-------------|
| CLI | `pi` | `pir` |
| 全局配置 | `~/.pi/agent` | **`~/.pir/agent`** |
| 项目配置 | `.pi` | **`.pir`** |
| 环境变量前缀 | `PI_*` | **`PIR_*`** |

子目录布局镜像 Pi（`sessions/`、`settings.json`、`extensions/`、`skills/` 等），仅根目录名不同。环境变量名按 APP_NAME 派生（如 `PI_CODING_AGENT_DIR` → `PIR_CODING_AGENT_DIR`）。详见 [ADR-0001](./adr/0001-extension-and-config-dir.md)。

不默认读写 `~/.pi` / `.pi`，**不提供**路径迁移工具。Session **文件格式**与钉死版 Pi JSONL 对齐。对照版本见 [`UPSTREAM.md`](../UPSTREAM.md) / [ADR-0002](./adr/0002-baseline-decisions.md)。

### 1.5 「1:1」的边界（术语定义）

本文档与姊妹文档中的「1:1」按以下三层理解；单独出现时默认指第 1 层：

1. **行为 1:1（对拍保证）**：事件序、session JSONL 格式与 v1–v3 迁移、KnownApi 协议适配、compaction 与 token 估算、CLI 标志与 slash 命令、声明式资源格式（Skills / Prompts / Themes / Settings / Keybindings / `models.json` / 凭据存储）、TUI 行为与渲染、RPC 帧语义。以 fixtures 对拍与黄金文件验收（见 §11）。
2. **API 形状同构（实现重写）**：扩展系统。ExtensionAPI 的形状、事件语义与能力面对齐，但扩展须以 Rust/Wasm 重写；**不兼容**现有 TS / jiti pi-package（[ADR-0001](./adr/0001-extension-and-config-dir.md)）。
3. **有意差异（ADR 钉死）**：CLI 名 `pir`、配置根 `~/.pir`、环境变量前缀 `PIR_*`（§1.4）；扩展包格式（Wasm 包替代 npm pi-package）；Rust SDK 替代 Node SDK；仅 JSONL 存储；无 `~/.pi` 迁移工具；范围排除项（§15）。（ADR-0001 / [ADR-0002](./adr/0002-baseline-decisions.md) / [ADR-0003](./adr/0003-coverage-review-scope-decisions.md)）

约束：新增行为级偏差只能通过新 ADR 进入第 3 层；不允许以「Rust 实现差异」为由放宽第 1 层。

---

## 2. 运行模式

### 2.1 Interactive（默认，TTY）

- 差分渲染 TUI：header、消息区、editor、footer
- Slash 命令、快捷键、消息队列（steering / follow-up）
- Project trust 交互提示
- 扩展 UI：dialog / widget / overlay / 自定义 editor
- interactive + piped stdin 或 stdout 非 TTY 时**自动降级为 print 模式**

### 2.2 Print（`-p` / 非 TTY 自动）

- 处理初始 prompt（含 piped stdin 合并）后，**依次发送全部消息**再退出
- 只打印**最后一条 assistant 消息的 text 块**（非 JSON）
- `stopReason` 为 `error`/`aborted` 时：错误信息写 stderr，**exit code = 1**
- SIGTERM/SIGHUP：杀 detached 子进程后分别以 **143/129** 退出
- 无 trust 提示；遵循 `defaultProjectTrust` / `--approve` / `--no-approve`

### 2.3 JSON（`--mode json`）

- stdout 首行输出**原样 SessionHeader 对象**（含 version/cwd），随后逐行输出 `AgentSessionEvent` JSONL
- 单向事件流，无命令环；同样先 `bindExtensions(mode: "json")`、支持多条顺序 prompt
- 事件全集：`AgentEvent` 基类 + `agent_end{willRetry}` / `agent_settled` / `queue_update` / `compaction_start|end` / `entry_appended` / `session_info_changed` / `thinking_level_changed` / `auto_retry_start|end` / `summarization_retry_*` / `bash_execution_update` / `extension_error`

### 2.4 RPC（`--mode rpc` 及独立入口 `pir-rpc`）

- stdin/stdout **严格 LF** JSONL：只按 `\n` 分帧（容忍行尾 `\r`），**不得**用按 Unicode 行分隔符（U+2028/U+2029）拆分的 reader
- 命令 + `type:"response"` + 异步 events；`prompt` 的成功响应在 preflight 通过后异步发出，失败走 error 响应；JSON 解析失败回 `command:"parse"` 错误
- **命令全集 32 个**（对齐 `docs/rpc.md`，逐条对拍）：
  `prompt`（含 images、streamingBehavior）、`steer`、`follow_up`、`abort`、
  `new_session{parentSession?}`、`get_state`、`get_messages`、
  `set_model`、`cycle_model`、`get_available_models`、
  `set_thinking_level`、`cycle_thinking_level`、`get_available_thinking_levels`、
  `set_steering_mode`、`set_follow_up_mode`、
  `compact{customInstructions?}`、`set_auto_compaction`、`set_auto_retry`、`abort_retry`、
  `bash{command,excludeFromContext?}`、`abort_bash`、`get_session_stats`、`export_html`、
  `switch_session`、`fork{entryId}`（position=before，返回 selectedText）、`clone`（=fork leaf, position=at）、
  `get_fork_messages`、`get_entries{since?}`、`get_tree`、`get_last_assistant_text`、`set_session_name`、`get_commands`
- 扩展 UI：`extension_ui_request` 9 种方法（select/confirm/input/editor/notify/setStatus/setWidget/setTitle/set_editor_text）；dialog 类阻塞等 `extension_ui_response{value|confirmed|cancelled}`（带 timeout/signal），fire-and-forget 类不等待
- **RPC 模式不可用/降级的 UI 能力**（逐条对齐）：`ui.custom()` 返回 undefined；`setFooter`/`setHeader`、`setWorkingMessage`/`setWorkingVisible`/`setWorkingIndicator`、`setHiddenThinkingLabel`、`onTerminalInput`、`addAutocompleteProvider`、`setEditorComponent` 不可用；theme 切换返回错误对象；`getEditorText` 恒返 `""`；`setWidget` 仅支持 string[]
- 关闭语义：扩展 `ctx.shutdown()` → 等 `agent_settled` 后退出；stdin EOF 即退；SIGTERM=143 / SIGHUP=129
- session 替换（new_session/fork/switch_session/clone）后 rebind 扩展与事件订阅
- `bash` 结果以 `` Ran `cmd` `` 格式的 user 消息进入后续上下文；`bash_execution_update` 事件带命令 id

### 2.5 SDK

- Rust crate API 对应 `createAgentSession` / `createAgentSessionRuntime` / `SessionManager` / `ModelRuntime` / `ResourceLoader`
- 跨语言嵌入优先走 RPC（与 Pi 给非 Node 宿主的建议一致）
- [DEFER] RPC 客户端库（对应 `rpc-client.ts`）可作为独立 crate 后置

---

## 3. CLI 需求

### 3.1 主命令标志（对齐 `cli/args.ts`）

| 标志 | 行为 |
|------|------|
| `--provider` / `--model` / `--api-key` | 选择模型与密钥。`--model` 支持 `provider/pattern` 与 `:thinking` 后缀简写（显式 `--thinking` 优先）；`--api-key` 必须搭配 model（否则 error），为非持久 runtime override |
| `--system-prompt` / `--append-system-prompt` | 覆盖/追加系统提示；可多次；**值为存在的文件路径时读文件内容，否则按内联文本** |
| `--mode text\|json\|rpc` | 输出模式 |
| `-p` / `--print` | 非交互；`-p` 后的值若不以 `@` 开头、且（不以 `-` 开头或以 `---` 开头）则被吞噬为 message；即 `---foo` 会被吞噬、`-foo`/`--foo` 不会 |
| `-c` / `--continue` | 继续最近会话 |
| `-r` / `--resume` | 选择历史会话 |
| `--session` | 三级解析：路径 → 本项目 id 精确/前缀 → 全局跨项目 id 前缀；跨项目命中时交互确认 fork 进当前目录 |
| `--session-id` | 精确 id 匹配（正则 `/^[A-Za-z0-9](?:[A-Za-z0-9._-]*[A-Za-z0-9])?$/`）；不存在则以该 id 新建；与 `--session/--continue/--resume` 互斥 |
| `--fork` | 与 `--session/--continue/--resume/--no-session` 互斥；可与 `--session-id` 组合（目标已存在则报错） |
| `--session-dir` / `--no-session` | 会话目录覆盖 / 内存会话 |
| `-n` / `--name` | 会话显示名（trim 非空校验，写 `session_info` 条目） |
| `--models` | Ctrl+P 循环模型列表；支持 glob（`anthropic/*`）、模糊匹配、`:thinking` 后缀；未指定回落 `settings.enabledModels` |
| `-t` / `--tools`，`-xt` / `--exclude-tools` | 工具 allowlist / denylist（deny 在 allow 之后应用） |
| `-nt` / `--no-tools`，`-nbt` / `--no-builtin-tools` | 全禁工具 / 只禁内置 |
| `--thinking` | `off\|minimal\|low\|medium\|high\|xhigh\|max`；非法值仅 warning 不退出 |
| `-e` / `--extension`（可多次）/ `-ne` / `--no-extensions` | 扩展路径 / 禁用发现 |
| `--skill`（可多次）/ `-ns` | skills |
| `--prompt-template`（可多次）/ `-np` | prompt templates |
| `--theme`（可多次）/ `--no-themes` | themes |
| `-nc` / `--no-context-files` | 禁用 AGENTS.md 等 |
| `--export <file> [output.html]` | 导出 HTML 后退出，不进入任何模式 |
| `--list-models [search]` | 列出模型（provider/model/context/max-out/thinking/images 表格），可选模糊搜索 |
| `--approve` / `-a`，`--no-approve` / `-na` | 本次运行信任 / 忽略项目本地文件（project trust） |
| `--verbose` | 覆盖 `quietStartup` 设置 |
| `--offline` / `-h` / `-v` | 离线（同时设 `PIR_SKIP_VERSION_CHECK`）/ 帮助 / 版本 |
| `@file` 位置参数（可多个） | 文本包 `<file name=...>` 标签；图片转 ImageContent（autoResize 2000×2000，空文件跳过，不存在 exit 1）；RPC 模式禁止 |
| **未知 `--flag`** | 收集为扩展标志（`extensionFlagValues`）透传扩展；help 动态追加 "Extension CLI Flags" 段；单个 `-x` 未知短选项为 error diagnostic |

诊断体系：args/settings 解析产生 warning/error diagnostics（带 scope 前缀），有 error 即 exit 1；扩展加载失败附 `pir -ne` 提示。

首次运行（interactive）：主题选择 + analytics opt-in。

### 3.2 子命令

| 命令 | 行为 |
|------|------|
| `pir install <source> [-l]` | source 类型：`npm:`（`name@version`，精确版本=pinned）、`git:`（`github.com/u/r`、`git@host:u/r` 简写）、`https://`、`ssh://`、本地路径；**裸名按本地路径解析**。安装后写入 settings；`-l` 写项目级 `.pir/settings.json` |
| `pir remove` / `uninstall <source> [-l]` | 卸载包（alias）；无匹配 exit 1 |
| `pir list` | User/Project 分组、`(filtered)` 标记、安装路径 |
| `pir update` | 裸命令 = 仅 self + 打印 "Extensions are skipped" 提示；`--self`/`pi`（alias，`--force` 强制重装，release `note` Markdown 渲染）、`--all`、`--extensions`、`--models`（刷新模型 catalog，15s 超时，互斥）、`--extension <source>`（仅一次）；互斥矩阵对齐 `package-manager-cli.ts`；`update` 只使用已保存 trust，永不弹窗 |
| `pir config [-l]` | TUI（Tab 切 global/project scope）启用/禁用 extensions、skills、prompts、themes；`-l` 要求项目已信任 |

所有子命令在 `parseArgs` 之前分流处理；各支持 `-a/-na` trust 覆盖与独立 `--help`。

> 实现注记（T14-W3，详见偏离 D-041/D-042）：`update` 全目标（self/extensions/all/单源）与
> `config` 已接线（`cli/package_command.rs`、`cli/config_command.rs`）；`config` TUI 复用
> T12 组件（输入换为包管理器全量 `resolve_all`，toggle 直写 settings）；`tui.select.cancel`
> 绑定 escape+ctrl+c 使上游显式 ctrl+c→onExit 分支不可达的 quirk 保留。

### 3.3 环境变量

进程级（文档级对齐 `docs/environment-variables.md`，名前綴 `PI_` → `PIR_`）：
`PIR_CODING_AGENT`（进程标记，SDK 嵌入不设）、`PIR_CODING_AGENT_DIR`、`PIR_CODING_AGENT_SESSION_DIR`、`PIR_PACKAGE_DIR`、`PIR_OFFLINE`、`PIR_SKIP_VERSION_CHECK`、`PIR_TELEMETRY`、`PIR_CACHE_RETENTION`、`PIR_SHARE_VIEWER_URL`、`PIR_HARDWARE_CURSOR`、`PIR_CLEAR_ON_SHRINK`、`PIR_EXPERIMENTAL`、`PIR_TIMING`、`PIR_STARTUP_BENCHMARK`、`PIR_DEBUG_REDRAW`、`PIR_TUI_WRITE_LOG`、`VISUAL`/`EDITOR`、`HTTP_PROXY`/`HTTPS_PROXY`/`no_proxy`、`GIT_TERMINAL_PROMPT`/`GIT_SSH_COMMAND`，及各 provider API key 变量（见 §5.6）。

bash 工具会话注入（仅 LLM bash 工具，**不注入**用户 `!`/`!!`；spawnHook 之前注入；未启用时删除继承的 `PIR_*` 防串味）：
`PIR_SESSION_ID`、`PIR_SESSION_FILE`、`PIR_PROVIDER`、`PIR_MODEL`、`PIR_REASONING_LEVEL`——每次命令启动时解析，模型切换即时生效。

### 3.4 平台文档级需求

Windows（bash 查找顺序：shellPath → Git Bash 固定路径 → PATH）/ Termux（termux-clipboard、无图片粘贴）/ tmux（extended-keys csi-u，两种格式字节序列均支持）/ terminal-setup（各终端 Kitty 协议适配）/ shell-aliases（`shellCommandPrefix` 注入 `shopt -s expand_aliases`）：行为与文档说明对齐（作为验收 checklist，见 §14 追溯表）。

---

## 4. Agent 运行时需求

### 4.1 消息模型

支持 `AgentMessage` 联合类型：

- 基础：`user` / `assistant` / `toolResult`（含 Text/Image/Thinking/ToolCall content blocks）
- 扩展：`bashExecution` / `custom` / `branchSummary` / `compactionSummary`

字段级要求：

- `bashExecution`：`command/output/exitCode(number|undefined)/cancelled/truncated/fullOutputPath?/timestamp/excludeFromContext?`；`excludeFromContext: true` 时 `convertToLlm` 直接过滤
- `ToolResultMessage`：`details?/usage?/addedToolNames?/isError`；`usage` 不计入主 LLM context 记账
- `AssistantMessage.stopReason` ∈ `stop|length|toolUse|error|aborted`（`pending` 存在于类型但仅瞬时；不落盘的保证来自「只在 message_end 持久化、partial 永不产生 message_end」）
- `AssistantMessage` 另含 `api/provider/model/responseModel?/responseId?/diagnostics?/usage/errorMessage/timestamp`
- `ToolCall.thoughtSignature?`（Google）、`TextContent.textSignature?`（OpenAI Responses item id）、`ThinkingContent.thinkingSignature?/redacted?`

**扩展消息 → LLM 的逐字文本格式**（provider payload 对拍必须一致）：

- `bashExecutionToText`：`` Ran `cmd` `` + fenced output + cancelled/exit code/truncated 后缀
- `branchSummary` / `compactionSummary`：固定 `BRANCH_SUMMARY_PREFIX/SUFFIX`、`COMPACTION_SUMMARY_PREFIX/SUFFIX` 包装（常量逐字移植）

### 4.2 事件模型

Agent 事件 10 种，**载荷结构同为契约**：

- `agent_start` / `agent_end{messages}` / `turn_start` / `turn_end{message, toolResults}`
- `message_start` / `message_update{message, assistantMessageEvent}`（仅 assistant，含 10 种流式子事件）/ `message_end`
- `tool_execution_start` / `tool_execution_update` / `tool_execution_end`
- toolResult 消息同样产生 message_start/end 对

流式边界：**不同 content block 的 start/delta/end 事件不保证连续**，消费者必须按 `contentIndex` 关联。

### 4.3 循环语义（对齐 `agent-loop.ts`）

1. `transform_context`（可选，不得抛异常）→ `convert_to_llm`（必选；Agent 类有默认 filter：user/assistant/toolResult）→ 每次 LLM 调用前**动态 `get_api_key`** → `stream_fn`；流式期间 partial 实时写 context 尾部，done/error 时替换为最终消息
2. 工具执行模式：`parallel`（默认）或 `sequential`；**batch 内任一工具声明 `executionMode:"sequential"` 则整批顺序执行**
3. parallel 精确语义：preflight（find → `prepareArguments` shim → schema 校验 → `before_tool_call` → abort 检查）**始终顺序**；immediate 结果（block/工具未找到/校验失败/abort）在 preflight 阶段即按源序 emit end；只有 prepared 的调用并发执行，`tool_execution_end` 按完成序；持久化 toolResult 的 message_start/end 与 `turn_end.toolResults` **按 assistant 源序**
4. `before_tool_call` 可 block；`after_tool_call` 五字段（content/details/isError/usage/terminate）**独立整体替换、无深合并**；钩子抛错降级为 error result
5. **`stopReason === "length"` 整批失败保护**：assistant 因 token 上限截断时，所有 tool call 一律不执行，各产出固定文案的错误 toolResult
6. 参数校验失败/工具未找到 → 错误 toolResult，不执行；`prepareArguments` 为 schema 校验前的原始参数兼容 shim（edit 用它归一 legacy `oldText/newText` 与 JSON 字符串化的 `edits`）
7. `terminate` 是 runtime-only hint（不写入 transcript）；batch 内**每个** finalized 结果都为 true 时跳过后续 LLM
8. steering 轮询点：**run 启动时一次** + 每个内层迭代末尾（`turn_end` → `prepareNextTurn` → `shouldStopAfterTurn` 之后）；注入时先发 message_start/end 再进 LLM
9. follow-up：agent 空闲后注入；drain 后继续外层循环
10. `steeringMode` / `followUpMode`：`one-at-a-time`（**默认**）| `all`；`one-at-a-time` 每次 drain 只取最老一条
11. `error`/`aborted` 提前返回：直接 `turn_end(toolResults: [])` + `agent_end`，不检查工具、不轮询队列
12. turn 边界：首个 turn 不重复发 `turn_start`；prompt 消息的 message_start/end 在 `turn_start` 之后
13. abort 细粒度：preflight 内多处 abort 检查产出 `"Operation aborted"` 错误结果；sequential 每个工具后检查并 break
14. `continue()` 降级链：last message 为 assistant 时先 drain steering 队列作为新 prompt（跳过首次 steering 轮询），再 drain followUp，都没有才抛错
15. 循环钩子：`prepareNextTurn`（turn_end 后可整体替换下一轮 context/model/thinkingLevel）；`shouldStopAfterTurn`（返回 true 直接 agent_end，自动 compaction 的官方触发机制）
16. `Agent` 订阅者 await 屏障：**全事件**先 reduce 内部状态，再按注册顺序 await 全部 listener；`agent_end` listener settle 前 `isStreaming` 保持 true、`waitForIdle()` 不 resolve；低层 `agentLoop()` 无屏障（observational EventStream）
17. `handleRunFailure`：loop 抛异常时合成空 content 的 failure assistant 消息（stopReason=aborted|error），补发完整事件序列
18. `tool_execution_update` 生命周期：execute settle 后的 onUpdate 被忽略；已排队 update 事件返回前全部 await
19. 互斥：`activeRun` 存在时 `prompt()`/`continue()` 抛错

### 4.4 Harness 层（完整移植，ADR-0003 §1）

对齐 `packages/agent/src/harness/`，作为 `pir-agent` 的可选层（`pir` CLI 主路径不经过它，但行为须可对拍）：

- `AgentHarness`：phase 状态机（idle/turn/compaction/branch_summary/retry）；turn snapshot vs config 分离（setters 立即生效但只影响下一 turn）；三队列（steer/followUp/**nextTurn**，nextTurn 于下次 prompt 并入头部）；错误归一化（`AgentHarnessError` 等结构化错误码，**能力层不抛异常、错误走 Result**）
- harness 事件 22 种：`queue_update` / `save_point` / `abort` / `settled` / `before_agent_start` / `context` / `before_provider_request` / `before_provider_payload` / `after_provider_response` / `tool_call` / `tool_result` / `session_before_compact` / `session_compact` / `session_before_tree` / `session_tree` / `retry_scheduled` / `retry_attempt_start` / `retry_finished` / `model_update` / `thinking_level_update` / `tools_update` / `resources_update`；subscribe/on 双订阅模型——`subscribe` 为纯观察 listener（支持 `*` 通配、无返回值），`on(type, handler)` 为带返回值的 hook；多 handler 顺序执行、最后一个非 undefined 结果胜出；`before_provider_request` 类 patch 型 hook 则把各 handler 的 patch 依次归约
- **持久化屏障（决定 JSONL 行序，对拍核心）**：`message_end` 时**先写 session 再发事件**；busy 期间写入进 `pendingSessionWrites` 队列；`turn_end` flush 后 emit `save_point`；`agent_end` flush + phase→idle + `settled`；屏障写入在 loop 事件回调内 await，持久化失败使 loop reject → emitRunFailure 合成失败 assistant 消息并重放完整事件序列（失败消息本身也走持久化）；emitRunFailure 内部再失败则聚合为 `AgentHarnessError`（"Agent run failed and failure reporting failed"）；executeTurn 末尾 finally flush 失败直接抛出、不经 emitRunFailure
- SessionStorage/SessionRepo 抽象 + JSONL（header **version: 3**，entry id=uuidv7 后 8 位碰撞重试）+ InMemory 实现；SQLite 不做（ADR-0002 §7），抽象同构预留；entryTransforms/entryProjectors 扩展点（session 条目写入前 transform、读取时 project，支持构造选项与 per-call 合并）；leaf 条目语义——leaf 移动靠追加 `type:"leaf"` 记录（parentId 指旧 leaf、targetId 指新 leaf）而非原地修改，加载时顺序重放全部条目重建 leafId
- harness 自带 compaction / branch summarization / skills 加载 / prompt-templates / 默认工具工厂（read/write/edit/bash）与 `streamProxy` SSE 协议（`/api/stream`；proxy SSE 事件 12 种：start, text_start, text_delta, text_end, thinking_start, thinking_delta, thinking_end, toolcall_start, toolcall_delta, toolcall_end, done, error）——均移植；与 coding-agent 实现的差异以 coding-agent 为准（ADR-0003 §2）

### 4.5 内置工具

**行为基准：coding-agent 实现**（`packages/coding-agent/src/core/tools/`，ADR-0003 §2）。
**默认启用**：`read`、`write`、`edit`、`bash`（默认激活集 `["read","bash","edit","write"]`）
**可选**：`grep`、`find`、`ls`

| 工具 | 关键行为锚点 |
|------|--------------|
| read | 文本/图像（jpg/png/gif/webp/bmp 魔数检测）；offset 1-indexed 越界报错；limit 先截取再过 truncateHead；截断提示附 nextOffset 续读指引；首行超 50KB 给 `sed -n 'Np' \| head -c 51200` 回退提示；图像 autoResize 2000×2000（`images.autoResize`），非视觉模型附省略提示；图像魔数检测含三条拒绝子规则（识别失败则按文本读取、不报错）：JPEG 第 4 字节 0xF7（SOF7）拒绝、PNG 在 IDAT 前出现 acTL chunk（APNG）拒绝、BMP 须过 DIB 头校验（长度≥26、DIB size ∈ {12, 40–124}、colorPlanes=1、bpp ∈ {1,4,8,16,24,32}）；`@` 前缀剥离与路径变体尝试——按序四类（文件不存在时逐个试、首个命中即返回）：macOS 截图名 `" (AM|PM)."` 前空格→U+202F、NFD Unicode 归一化、直引号 '→弯引号 U+2019、NFD+弯引号组合；基线另有 Unicode 空格归一化 |
| write | utf-8 写入；递归创建父目录；返回 `Successfully wrote N bytes` |
| edit | `edits[]` 多块替换：全部针对原始文件匹配、逆序应用；fuzzy 归一化（NFKC + 行尾空白 + 智能引号/破折号/特殊空格→ASCII）；唯一性在 fuzzy 空间校验（重复报错并给出现次数）；重叠/嵌套/空 oldText/无变化四类错误文案；BOM 剥离写回、CRLF/LF 检测还原；fuzzy 命中时按行 overlay 保留未改行原始字节；diff 上下文 4 行（diff + unified patch + firstChangedLine）；legacy 兼容 shim 两类强转：`edits` 为 JSON string 时尝试 JSON.parse 还原为数组（上游注释点名 Opus 4.6 / GLM-5.1 会这样发）、顶层 legacy oldText/newText 折叠进 edits[] |
| bash | **无默认超时**（上限 2³¹−1 ms）；stdout+stderr 合流；tail 截断 2000 行/50KB，超量全量写 `tmpdir/pi-bash-<hex>.log`（滚动缓冲 2×50KB）；返回给 LLM 的输出为原始 UTF-8 解码文本，**不做控制字符清洗**（`\r` 去除/控制字符清洗只发生在 TUI 渲染层与用户 `!`/`!!` bash-executor）；detached 进程组 + 杀进程树取消；onUpdate 100ms 节流；非零退出码抛错附输出；`shellPath`/`shellCommandPrefix` 定制；spawnHook 扩展点；会话环境注入（§3.3） |
| grep | 行为等价 rg：`--json --line-number --color=never --hidden` 语义；默认 limit=100 匹配（达标即停）；单行截 500 字符；context>0 回读文件补上下文；50KB 字节截断；提示含 `limit=N*2` 翻倍建议（ADR-0003 §2：Rust 原生实现，不下发外部 rg）；输出分隔符仿 grep 惯例——匹配行 `path:lineno: text`，上下文行 `path-lineno- text`（context=0 时全部 `:` 格式） |
| find | 行为等价 fd：`--glob --color=never --hidden` 语义；git repo 外自动 `--no-require-git` 等价；pattern 含 `/` → full-path 且自动补 `**/` 前缀；默认 limit=1000；相对化输出、保留目录尾斜杠；固定忽略 node_modules/.git |
| ls | 默认 limit=500；大小写不敏感排序（Rust 实现对小写后名称按码位排序，替代上游 `localeCompare` 的 ICU 排序，纯 ASCII 字母数字名称一致，见 D-039）；目录加 `/`；含 dotfiles；stat 失败跳过；50KB 字节截断；提示 `limit=N*2` |

公共：截断常数 `DEFAULT_MAX_LINES=2000`、`DEFAULT_MAX_BYTES=50KB`、`GREP_MAX_LINE_LENGTH=500`；truncateHead 不截整行（首行超限返回 firstLineExceedsLimit）；truncateTail 末行可部分截断（UTF-8 边界感知）；write/edit 经 file mutation queue 按 **realpath** 串行化（ENOENT 退化 resolve 路径；abort 不在事件回调里 reject）；工具可插拔 operations（ReadOperations/BashOperations 等，供扩展/沙箱改道）。

用户 `!`/`!!` bash 走**独立 bash-executor 路径**（非工具）：滚动缓冲 2×50KB、超 50KB 开临时文件；stripAnsi + 二进制清洗 + 去 `\r`；无超时参数；**不注入 `PIR_*` 会话变量**；`!!` 置 `excludeFromContext`；结果存 `bashExecution` 消息，流式期间挂起、agent_end 时 flush（保 tool_use/tool_result 顺序）。

工具控制：`--tools/-t` allowlist、`--exclude-tools/-xt` denylist（deny 后于 allow）、`--no-tools/-nt` 全禁、`--no-builtin-tools/-nbt` 只禁内置；扩展工具可同名覆盖内置（`Map.set` 语义）。

---

## 5. LLM / Provider 需求

### 5.1 KnownApi（必须实现，10 个）

- `openai-completions`
- `openai-responses`
- `azure-openai-responses`
- `openai-codex-responses`
- `anthropic-messages`
- `google-generative-ai`
- `google-vertex`
- `bedrock-converse-stream`
- `mistral-conversations`
- `pi-messages`

### 5.2 协议适配器行为锚点（逐适配器对拍）

- **openai-completions**：**compat URL 自动检测矩阵**（zai/together/moonshot/openrouter/cloudflare/nvidia/ant-ling/cerebras/xai/chutes/deepseek/opencode 等 → `supportsStore/developerRole/reasoningEffort/maxTokensField/strictMode/thinkingFormat/sessionAffinityFormat/longCacheRetention` 默认值；`model.compat` 部分覆盖时未设置字段回落检测值）；10 种 `thinkingFormat`；`prompt_cache_key`（sessionId 截 64）与 `prompt_cache_retention:"24h"`；`store:false`；`stream_options.include_usage`；usage 兜底读 `choice.usage`（Moonshot）；OpenRouter 路由偏好全字段 + `cacheControlFormat:"anthropic"` + `x-session-id`；grammar tools（lark 优先于 regex，单调增量校验）；`zaiToolStream`；Kimi deferred tools 序列化；session affinity 三格式；compat 字段全集 21 个：supportsStore, supportsDeveloperRole, supportsReasoningEffort, supportsUsageInStreaming, maxTokensField, requiresToolResultName, requiresAssistantAfterToolResult, requiresThinkingAsText, requiresReasoningContentOnAssistantMessages, thinkingFormat, chatTemplateKwargs, openRouterRouting, vercelGatewayRouting, zaiToolStream, supportsOpenAIGrammarTools, supportsStrictMode, cacheControlFormat, sendSessionAffinityHeaders, deferredToolsMode, sessionAffinityFormat, supportsLongCacheRetention；`thinkingFormat` 10 个取值：openai, openrouter, deepseek, together, zai, qwen, chat-template, qwen-chat-template, string-thinking, ant-ling
- **openai-responses**：encrypted reasoning 持久化（终态回填缺失字段）；`TextSignatureV1`（item id + phase）；tool_search 延迟工具；`prompt_cache_options`；`max_output_tokens` 下限 16；tool call id 为 `call_id|item_id` 复合格式，跨模型（foreign）item id 以 `fc_<shortHash>` 重建（截 64）并强制 `fc_` 前缀；compat 结构 7 字段：supportsDeveloperRole（默认 true）、sessionAffinityFormat（openai/openai-nosession/openrouter）、supportsLongCacheRetention（默认 true，控制 `prompt_cache_retention:"24h"`）、supportsStrictMode、supportsOpenAIGrammarTools、supportsToolSearch、supportsExplicitPromptCacheMode（后三默认 false）；azure-openai-responses 与 openai-codex-responses 复用同一 compat 类型
- **openai-codex-responses**：WebSocket 传输（按 sessionId 连接缓存：5min 空闲 TTL / 55min 最大年龄；WS 失败该 session 永久回退 SSE；连接数超限与 `previous_response_not_found` 各重试一次）；SSE 路径 **zstd 压缩请求体**（level 3，`Content-Encoding: zstd`）；`chatgpt-account-id`（从 JWT claim 解析）；`originator: "pi"`（**字节级对齐，实现时不得写成 `pir`**）；User-Agent 同样以 `pi (` 开头（`pi (<platform> <release>; <arch>)`）；`store:false` + `instructions` + `include:["reasoning.encrypted_content"]` + `text.verbosity:"low"`；service tier 价格乘数（flex ×0.5、priority ×2/×2.5）；WebSocket 缓存续传机制——以 lastRequestBody.input + lastResponseItems 为基线做前缀校验后计算 input delta，有效且有 lastResponseId 时发送 `{previous_response_id, input: delta}`
- **azure-openai-responses**：`AZURE_OPENAI_BASE_URL` 归一化（自动补 `/openai/v1`）/ `AZURE_OPENAI_RESOURCE_NAME`；`AZURE_OPENAI_API_VERSION` 默认 `v1`；`AZURE_OPENAI_DEPLOYMENT_NAME_MAP`；options 新增 6 字段（reasoningEffort/reasoningSummary + Azure 专属 azureApiVersion/azureResourceName/azureBaseUrl/azureDeploymentName）；Azure host 按 3 后缀识别（`.openai.azure.com`/`.cognitiveservices.azure.com`/`.ai.azure.com`），命中且路径为空/`/`/`/openai`/`/openai/v1/responses` 时归一化为 `/openai/v1`
- **anthropic-messages**：OAuth token（含 `sk-ant-oat`）走 **Claude Code 身份伪装**（system 前缀注入、`user-agent: claude-cli/…`、`x-app: cli`、beta 头）+ **工具名 canonical 大小写映射表**；自适应 thinking（`output_config.effort`）vs 预算 thinking（`budget_tokens`）双轨；`thinkingDisplay` 默认 `"summarized"`；interleaved/fine-grained beta 头；`cache_control: ephemeral`（long → `ttl:"1h"`）；`x-session-affinity`；usage 从 `message_start` 即捕获（abort 也有 input 计数）；stop 映射含 `refusal`（→error）/`pause_turn`（→stop）；tool call id 归一化（白名单字符、截 64）；deferred tools 用 `tool_reference`（默认规则：仅一方 Anthropic 模型、排除 Haiku 与 <4.5）；伪装字面值：Claude Code 版本 `2.1.75`（UA `claude-cli/2.1.75`）、beta 头 `claude-code-20250219` 与 `oauth-2025-04-20`、OAuth token 时注入的 system 前缀字面值 "You are Claude Code, Anthropic's official CLI for Claude."；工具名 canonical 映射 17 条（lowercase→canonical 双向）：Read, Write, Edit, Bash, Grep, Glob, AskUserQuestion, EnterPlanMode, ExitPlanMode, KillShell, NotebookEdit, Skill, Task, TaskOutput, TodoWrite, WebFetch, WebSearch
- **google-generative-ai**：thinking 按模型族分流（Gemini 3 Pro/Flash、Gemma 4 用 `thinkingLevel` 且**无法真正关闭**——关时用最低 level 不带 `includeThoughts`；其余用 `thinkingBudget`，-1 动态）；`thoughtSignature` 保留；budget 档位表（minimal/low/medium/high）：2.5-pro=128/2048/8192/32768、2.5-flash-lite=512/2048/8192/24576、2.5-flash=128/2048/8192/24576、其余 -1（动态），options.thinkingBudgets 覆盖优先；usage 映射 input=promptTokenCount−cachedContentTokenCount、output=candidatesTokenCount+thoughtsTokenCount、cacheRead=cachedContentTokenCount、cacheWrite=0、reasoning=thoughtsTokenCount、totalTokens=totalTokenCount；function calling `VALIDATED` 模式；不支持函数调用流式（单个 toolcall_delta 给全量）；tool call id 自增生成
- **google-vertex**：API key 或 ADC；project/location 解析；baseUrl 含 `{location}` 占位符时被**整体丢弃**（resolveCustomBaseUrl 返回 undefined）、回退 SDK 默认端点，不做模板替换
- **mistral-conversations**：`promptMode:"reasoning"` vs `reasoningEffort` 按模型二选一；tool call id 归一化为 9 字符纯字母数字（hash + 碰撞重试）；`x-affinity`/`promptCacheKey`；cached tokens 6 种字段名变体（按序：promptTokensDetails.cachedTokens、prompt_tokens_details.cached_tokens、promptTokenDetails.cachedTokens、prompt_token_details.cached_tokens、numCachedTokens、num_cached_tokens；结果钳制 [0, promptTokens]；input = prompt − cached、cacheRead = cached）
- **bedrock-converse-stream**：SigV4 vs bearer token；region 解析顺序（model ARN → 配置 → endpoint → `us-east-1`）；header 白名单（`x-amz-*`/`authorization`/`host` 忽略）；`cachePoint`（long → 1h TTL）；thinking 走 `additionalModelRequestFields`；Claude 4.x interleaved thinking；空白文本占位 `EMPTY_TEXT_PLACEHOLDER = "<empty>"`（必填文本块/tool result 空内容/消息空 content 三处，Bedrock 拒绝空白文本）；自适应 thinking 模型族子串清单（id 与 name 归一化小写、`[\s_.:]+`→`-` 后匹配）：opus-4-6/4-7/4-8/5、sonnet-4-6/5、fable-5；原生 xhigh effort 族为其子集（去掉 opus-4-6、sonnet-4-6）
- **pi-messages**：单 POST `<baseUrl>/messages`，SSE 回传事件 + 终态 done/error；`contentSignature/redacted/rewrite`（rewrite → diagnostics）；`debug=1` query

### 5.3 Providers（必须具备等价能力，38 个内置工厂）

amazon-bedrock、ant-ling、anthropic、azure-openai-responses、cerebras、cloudflare-ai-gateway、cloudflare-workers-ai、deepseek、fireworks、github-copilot、google、google-vertex、groq、huggingface、kimi-coding、minimax、minimax-cn、mistral、moonshotai、moonshotai-cn、nvidia、openai、openai-codex、opencode（Zen）、opencode-go、openrouter、qwen-token-plan、qwen-token-plan-cn、radius、together、vercel-ai-gateway、xai、xiaomi、xiaomi-token-plan-cn/-ams/-sgp、zai、zai-coding-cn。

机制要求：

- **混合 API provider** 按 `model.api` 分发（github-copilot 3 API + `filterModels` 按 OAuth 侧 `availableModelIds` 过滤；opencode 4 API；opencode-go/xai/fireworks 各 2–3 API）
- `createProvider` 动态 catalog 机制（radius 纯动态；llama.cpp 为 coding-agent 扩展，见 §9）；动态 overlay 与 baseline 按 id 合并；`refreshModels` 并发去重
- cloudflare baseUrl 占位符（`{CLOUDFLARE_ACCOUNT_ID}` 等）分发前物化；AI Gateway 用 `cf-aig-authorization` 并删除 `Authorization`/`x-api-key`
- 内置模型目录为**生成物**（数据源 models.dev）：`build.rs` 生成内置 + `pir update --models` 远程 overlay（`https://<endpoint>/api/models/providers/<id>`，ETag/If-None-Match、4 小时新鲜度、本地 generatedAt 比对）；`ModelsStore` 持久化（models/lastModified/checkedAt/etag），`refresh({allowNetwork:false})` 离线恢复、`force` 跳过新鲜度
- 远程 catalog endpoint 可配置（ADR-0002 §8）

### 5.4 Auth

- **解析顺序**：显式 `options.apiKey` → **credential store（命中即停，拥有 provider）** → ambient（env var / AWS profile / ADC）。OAuth 是 credential 的一种类型；过期时在 `modify` 锁内双重检查刷新；**刷新失败抛错且绝不静默回退 env key**
- CredentialStore 契约：`read/list/modify/delete`；`modify` 是唯一写路径（按 provider 串行化 read-modify-write + 跨进程文件锁）；`list()` 只返回 `{providerId,type}` 不解析密钥；credential 判别式 `{type:"api_key",key?,env?} | {type:"oauth",refresh,access,expires,...}` 与 Pi `auth.json` 兼容（0600）
- env 变量表逐 provider 对齐（`docs/providers.md` 33 家对照表，上游 pi 仓库文档；pir 侧权威对照表为 `env_keys.rs` 全表）；Anthropic 三变量优先级 `ANTHROPIC_AUTH_TOKEN` > `ANTHROPIC_OAUTH_TOKEN` > `ANTHROPIC_API_KEY`（`ANTHROPIC_AUTH_TOKEN` 命中时产生 `Authorization: Bearer <token>` 头，`ANTHROPIC_OAUTH_TOKEN`/`ANTHROPIC_API_KEY` 走 apiKey（x-api-key）路径）
- **key 值解析 DSL**（auth.json 与 models.json 通用）：`!cmd` 执行命令取 stdout、`$VAR`/`${VAR}` 插值、`$$`/`$!` 转义
- **OAuth 流程 7 个**：anthropic（PKCE + 本地回调与 manual_code 竞速）、openai-codex（PKCE、`id_token_add_organizations`、originator）、github-copilot（**device code**，enterprise 域名、per-account baseUrl、登录时拉 `availableModelIds`；登录成功后对每个已知模型 POST `${baseUrl}/models/{id}/policy`（body `{state:"enabled"}`，头 `openai-intent: chat-policy`）做 policy-enable，缺失会导致 Claude/Grok 等模型首次登录后不可用）、openrouter（PKCE 换**永久 API key**，refresh no-op）、kimi-coding、xai、radius；device code 流程实际覆盖 5 个 provider——github-copilot、kimi-coding、xai、radius（均为 RFC 8628 device_code grant）+ openai-codex（OpenAI 私有 deviceauth 端点变体，`/api/accounts/deviceauth/usercode|token`，验证 URI `/codex/device`）；device 轮询遵循 RFC 8628（默认 5s、slow_down +5s、下限 1s、WSL 时钟漂移错误信息）
- provider 自有 login：Bedrock（bearer-token/aws-profile/credential-chain）、Vertex（api-key/adc/service-account）、Cloudflare（多字段 prompt 存 `credential.env`）
- `/login` `/logout` 订阅流；`checkAuth()` / `getAvailable()`
- 交互协议：`AuthInteraction.prompt()`（text/secret/select/manual_code，per-prompt signal 竞速取消）+ `notify()`（links/auth_url/device_code/progress）
- `options.env` 每请求环境覆盖（Cloudflare/Azure/Vertex/Bedrock/代理变量都走它）
- （Rust 落地注记：auth 存储/DSL/OAuth 框架的实现细节差异——fs2 锁语义边界、`!cmd` 仅 unix、快照保序、device 时钟抽象、OAuth 测试缝等，见偏离 D-008 / D-009；T13 W5 六个 OAuth 流程的落地差异见 D-033 / D-034 / D-035）

### 5.5 横切能力

- **stream 不抛出契约**：stream 一旦调用，所有失败（含 auth、abort、tool 校验）编码为 error 事件 + `stopReason:"error"/"aborted"`；aborted 部分消息可继续对话
- Thinking 统一级别：`off|minimal|low|medium|high|xhigh|max`（`streamSimple` 层 `reasoning` 无 `off`，off = 省略）；`thinkingBudgets` 仅 minimal/low/medium/high 四档；`clampThinkingLevel`（先上后下找最近可用）；xhigh/max 在预算路径降为 high；默认预算 minimal 1024 / low 2048 / medium 8192 / high 16384，minOutput 1024
- **maxTokens 钳制**：`contextWindow − 估算context − 4096` 安全余量，下限 1
- Image input（非 vision 模型替换占位文本：`(image omitted: model does not support images)` / `(tool image omitted: …)`）；image generation（独立 ImagesModels 子系统，OpenRouter images 走 chat completions `modalities:["image","text"]`，永不 reject）
- **transformMessages（cross-provider handoff）**：孤儿 tool call 合成 `"No result provided"` 的 isError toolResult（含结尾悬空）；error/aborted assistant 消息整体跳过不回放；redacted thinking 跨模型丢弃；thoughtSignature 跨模型删除；跨模型 thinking → 纯文本；tool call id 归一化映射回填 toolResult
- **两层重试**：provider 层（`x-should-retry` header 优先、408/409/429/5xx、retry-after 优先级链 `retry-after-ms`（毫秒）→ `retry-after` 秒数 → `retry-after` HTTP-date（Date.parse − now）→ 指数退避 min(0.5·2ⁿ,8)s·(1−rand·0.25)、可中断 sleep、仅服务端指定延迟受 `maxRetryDelayMs`（默认 60s，0 关闭）上限约束、**超限立即失败**并把秒数写进错误信息；Codex 直连路径独立实现同一优先级链并额外 Math.max(0,…) 钳制）+ 外层 `retryAssistantCall`（baseDelay·2ⁿ、两张大 regex 表区分瞬时 vs 配额/账单不可重试）
- Token / cost / cache 统计：`calculateCost` 支持 `cost.tiers` 阶梯定价（tier 匹配口径 inputTokens = input + cacheRead + cacheWrite（**不含 output**），取满足 inputTokens > inputTokensAbove 的最高阈值 tier，命中 tier 的全套费率 request-wide 应用）；cacheWrite 拆分——长时档 cacheWrite1h（仅 Anthropic 上报，`ephemeral_1h_input_tokens`）按 2× input 基础费率（硬编码），短时档 = cacheWrite − cacheWrite1h 按 cacheWrite 费率，cost.cacheWrite = (rates.cacheWrite·short + rates.input·2·long)/1e6；Codex service tier 乘数；totalTokens 由分量合成
- **overflow 检测三分支**：错误文本 pattern 表（含非溢出排除表）/ z.ai 静默溢出（stop 但 input+cacheRead > window）/ Xiaomi 截断式（length + output=0 + input≥99% window）
- `sessionId` 选项：驱动 prompt_cache_key、session affinity headers、Codex WS 连接复用、faux cache 模拟
- `cacheRetention`（`none|short|long`，默认 short）：Anthropic ephemeral/1h、OpenAI 24h、Bedrock cachePoint、Mistral promptCacheKey；`PIR_CACHE_RETENTION=long` 遗留 env
- Transport 偏好：`sse|websocket|websocket-cached|auto`（设置项；仅 openai-codex-responses 实现 WebSocket，其余静默忽略）
- 钩子：`onPayload`（可替换 payload）/ `onResponse` / `transformHeaders`（Models 层分发前剥离）；header 大小写不敏感合并、`null` 值删除低层默认
- 校验：工具参数 JSON Schema 运行时校验双路径——schema 带 TypeBox symbol 时只做 Value.Convert 前奏 + Compile 校验，纯 JSON-Schema 时额外递归自定义强转；强转表：number←{null→0, 非空数字字符串→Number, bool→1/0}；integer←同 number 但仅接受整数值（isInteger vs isFinite）；boolean←{null→false, "true"/"false", 1/0}；string←{null→"", number/bool→String}；null←{""/0/false→null}；组合递归：allOf 逐支、anyOf/oneOf 取首个校验通过分支（clone 尝试）、type 数组按序取首个可转、object 按 properties/additionalProperties、array 按 items（tuple 对位）；流式 partial JSON 解析 + repair（控制字符转义、非法反斜杠加倍）；`sanitizeSurrogates` 去孤立 surrogate（Rust 落地注记：jsonschema 单路径替代 TypeBox 双路径、校验措辞差异、sanitize 恒等——见偏离 D-006 / D-007）
- Tool `constrainedSampling`：`{type:"json_schema",strict:"prefer"|"require"}` 或 `{type:"grammar",variants:{openai_lark?,openai_regex?}}`
- deferred tools：`ToolResultMessage.addedToolNames`、`splitDeferredTools`、各协议回退（Anthropic tool_reference / Kimi 序列化 / OpenAI tool_search）
- 诊断：`AssistantMessage.diagnostics` 类型字面值三种——`provider_transport_failure`、`pi_messages_rewrite`、`pi_messages_response_failure`；`redacted` 是 `ThinkingContent.redacted` 布尔字段、**不属于** diagnostics；`responseModel`/`responseId` 回填（OpenRouter auto 路由实际模型）
- 代理：`HTTP_PROXY/HTTPS_PROXY/no_proxy` 解析（Codex fetch/WS 使用）
- Faux provider（测试）：脚本化响应队列（空队列固定错误文案）、响应工厂、`tokensPerSecond` 节奏、usage 4 字符/token 估算、sessionId+cacheRetention≠none 时模拟 cache 读写、`state.callCount`

### 5.6 Provider 环境变量

逐 provider 对齐 `docs/providers.md` 对照表（上游 pi 仓库文档，未随 `external/pi` vendored 子集落地；pir 侧权威对照表为 `crates/pir-ai/src/auth/env_keys.rs` 全表）与 `env-api-keys.ts`（含各区域变体：`QWEN_TOKEN_PLAN_API_KEY`/`_CN_`、`ZAI_API_KEY`/`ZAI_CODING_CN_API_KEY`、`MINIMAX_API_KEY`/`MINIMAX_CN_API_KEY`、Xiaomi 四端点等；Moonshot 双 provider 共用 `MOONSHOT_API_KEY`）。

---

## 6. Session 需求

### 6.1 存储

- 第一版 **仅 JSONL**（不做 SQLite；存储抽象同构预留，ADR-0003）
- 路径：`~/.pir/agent/sessions/--<cwd>--/<timestamp>_<uuid>.jsonl`；目录编码 = **去前导斜杠后**把 `/`、`\`、`:` 全部替换为 `-`；文件名 timestamp 的 `:`/`.` → `-`
- 可覆盖：`--session-dir` / `PIR_CODING_AGENT_SESSION_DIR` / `settings.sessionDir`
- 版本迁移：v1 → v2（加 id/parentId；compaction 的 `firstKeptEntryIndex` 数字下标 → `firstKeptEntryId`）→ v3（message role `hookMessage` → `custom`）；加载时自动迁移并**整文件重写**；**分叉点**——「v1–v3 自动迁移」仅是主路径 SessionManager 的行为，harness 的 JsonlSessionStorage 硬性要求 header version === 3，非 v3 直接抛 invalid_session，不做 v1/v2 迁移
- session id = uuidv7；entry id = randomUUID 前 8 位（hex），碰撞重试 100 次退回完整 UUID
- Header：`{type:"session", version, id, timestamp(ISO), cwd, parentSession?}`
- **延迟落盘**：首个 assistant 消息出现前不创建文件（`flushed` 标志 + `wx` 独占创建）
- **无文件锁**（append 直写，`wx` 首次创建保护）；锁仅用于 auth.json/settings/trust（proper-lockfile 等价物）
- 读取健壮性：1MB 缓冲流式读、跳过畸形行；header 专用 4KB 缓冲/1MB 扫描上限（超限回退全量加载）
- `--no-session` 内存会话
- **不做** `~/.pi` → `~/.pir` 迁移工具；**不实现** Pi migrations.ts 的 legacy 启动迁移（ADR-0003 §3）

### 6.2 条目类型（header + 9 种，另有 harness 独有 2 种）

主路径：`session`(header) / `message` / `model_change` / `thinking_level_change` / `compaction` / `branch_summary` / `custom` / `custom_message` / `label` / `session_info`。

- `custom` 与 `custom_message` 的关键区别：前者**不进** LLM 上下文，后者参与
- `compaction` 兼容两种形态：含 `firstKeptEntryId` 的主路径形态（coding-agent 只写/读这种）与内嵌 `retainedTail` 的自包含形态（harness 产物，读写支持，ADR-0003 §1）
- harness 独有：`active_tools_change` / `leaf`（仅 harness 层读写）

### 6.3 上下文重建算法

- `buildContextEntries`：取路径上**最后一个** compaction；输出 = compaction 条目 + 从 `firstKeptEntryId` 起的条目 + compaction 之后的条目（retainedTail 形态用内嵌 tail）
- `buildSessionContext`：model 取自路径上最后 assistant 消息或 `model_change`；thinkingLevel 默认 `"off"`
- 树：`getTree` 子节点按 timestamp 排序、孤儿当根
- `createBranchedSession`：抽取单路径到新文件，**label 条目剔除并按重新链接的 parentId 重建**
- `forkFrom`：新 header（parentSession 指向源）+ 原样拷贝全部条目（保留原 id/parentId），`wx` 防覆盖；fork 的 `position: before|at` 语义与 user-message 校验

### 6.4 操作

- 继续 / 恢复 / 按 id 打开（`--session` 三级解析，§3.1）
- `/tree` 原地分支导航 + 可选 branch summarization；选中行为：选 user/custom_message → leaf 移到 **parentId** 且文本回填编辑器；选 assistant 等 → 移 leaf 留空编辑器；选根 user → 重置 leaf
- `/fork`（position=before，返回 selectedText）、`/clone`（fork leaf, position=at）、CLI `--fork`
- `/new`、import/export JSONL、HTML export、gist share（shell 调 `gh gist create --public=false` + `PIR_SHARE_VIEWER_URL` 拼接，默认 `https://pi.dev/session/` 可配）
- `/name`（写 `session_info` 条目，`\r\n`→空格 sanitize）、label/bookmark（空值清除）
- `/resume` 选择器内 Ctrl+D 删除（优先 `trash` CLI）

### 6.5 Compaction（算法逐字节对齐钉死版）

- **Token 估算**（同一算法与常量，禁止偏差）：`ceil(chars/4)`；image 块按 4800 chars；assistant toolCall 按 `name.length + JSON.stringify(args).length`；bashExecution 按 command+output；summary 类按 summary.length
- **上下文 token**：`calculateContextTokens` = `totalTokens || input+output+cacheRead+cacheWrite`；`estimateContextTokens` = 最后一条**有效** assistant usage（跳过 aborted/error/全零）+ 其后消息估算（trailingTokens）
- **触发双路**：agent_end 后 + **每次 prompt 提交前**（捕获 aborted 响应的超窗）；条件 `contextTokens > contextWindow - reserveTokens`
- **overflow 恢复**：三分支判定（§5.5）；仅同模型触发；只尝试一次；失败发 compaction_end 错误；`stopReason=stop` 的 overflow 只压缩不重试；stale pre-compaction usage 时间戳守卫防重触发
- 参数：`compaction.enabled` / `reserveTokens`(16384) / `keepRecentTokens`(20000)；`branchSummary.reserveTokens`(16384) / `branchSummary.skipPrompt`(false)
- **切点 `findCutPoint`**：从 newest 倒序累积估算 token ≥ keepRecentTokens 即停；合法切点 = user/assistant/bashExecution/custom/branchSummary/compactionSummary（**绝不切 toolResult**；切 assistant 时其 toolResult 自然保留）；再前向吸收无上下文元数据条目（遇 compaction 边界停）；非 turn-start → split turn（记录 turnStartIndex）
- **三个 summary prompt**（逐字移植）：`SUMMARIZATION_PROMPT`（Goal/Constraints/Progress/Key Decisions/Next Steps/Critical Context）、`UPDATE_SUMMARIZATION_PROMPT`（`<previous-summary>` 包裹迭代合并）、`TURN_PREFIX_SUMMARIZATION_PROMPT`（split turn 前缀）；customInstructions 以 `Additional focus:` 追加；所有 summary 调用共用 system prompt `SUMMARIZATION_SYSTEM_PROMPT`（"You are a context summarization assistant..."，须**字节级对齐**）；split-turn 合并格式串字面值 `\n\n---\n\n**Turn Context (split turn):**\n\n`；history 为空时的占位串 "No prior history."
- **预算**：history summary maxTokens = min(0.8×reserveTokens, model.maxTokens)；turn prefix = 0.5×reserveTokens
- 序列化：`<conversation>` 包裹；`[User]:`/`[Assistant tool calls]:` 等格式；tool result 截 2000 chars；文件操作跟踪 read/write/edit → 尾部 `<read-files>`/`<modified-files>` XML + `details.{readFiles,modifiedFiles}` 跨 compaction 累积
- 请求隔离：`cacheRetention:"none"` + 每次新 uuidv7 routing session id + 复用 `settings.retry` 重试（三类 `summarization_retry_*` 事件）；reasoning 模型带 thinkingLevel
- 重复压缩从上次 kept boundary 起算并重算 `tokensBefore`
- auto-compaction 完成后若 follow-up/steering 队列非空则 `agent.continue()` 一次
- 手动 `/compact [instructions]`；扩展可自定义（`session_before_compact` 可 cancel 或整体接管）
- **branch summary**：公共祖先查找；倒序装填至 `contextWindow − reserveTokens` 预算（compaction/branch_summary 条目 90% 预算内强行保留）；maxTokens 固定 2048；preamble 前缀；label 可挂 summary 条目；`session_before_tree`/`session_tree` 钩子

### 6.6 与 Pi 会话互通的降级策略

加载 Pi 生成的 session（含 TS 扩展产物）时：

- **保留**：所有 entry（含 `custom`、`label`、未知类型）原样保留在 session 树中，写回时不丢数据。
  - 边界（T07 落地，D-012）：保留为 JSON 语义级（与上游 `JSON.parse`/`stringify` 同级）；合法 JSON 但非对象的行（`42`/`"s"`）加载即丢弃；已知 type 但字段形状不合法的条目降级为 Raw 保留（写回无损，但退出 LLM context 与 model 推导）；未知字段内数字格式化有 `1e2`→`100.0` 级微差。
- **跳过 LLM context**：无对应扩展的 `custom` message/entry 不进入 `convert_to_llm` 输出；`bashExecution` 按钉死版 Pi 的 `convertToLlm` 规则处理。
- **通用渲染**：TUI 中未知 custom entry 以通用 JSON 折叠块渲染（类型名 + 数据摘要），不报错、不阻断会话。

---

## 7. 资源与定制

### 7.1 Context files

- 每目录候选按优先级：`AGENTS.md` > `AGENTS.MD` > `CLAUDE.md` > `CLAUDE.MD`，取第一个命中
- 加载顺序：全局 agentDir 一份 → 从 cwd 到**文件系统根**的完整祖先链（根侧在前，**不以 git repo root 为界**），按路径去重
- 注入格式：system prompt 尾部 `<project_context>` 内每文件包 `<project_instructions path="...">`
- `-nc` 禁用；SDK 有 override；**无论 trust 与否都加载**
- `.pir/SYSTEM.md` / `APPEND_SYSTEM.md`：覆盖/追加系统提示；**项目版需 trust 通过且优先于全局**；CLI `--system-prompt`/`--append-system-prompt` 支持文件路径或内联文本

### 7.2 Skills

- Agent Skills 标准（Pi 宽松规则：name 可≠目录名，**仅 warning 不拒绝**）
- **发现路径全集**：`~/.pir/agent/skills/`、`~/.agents/skills/`、`.pir/skills/`（trust 门控）、cwd 及祖先 `.agents/skills/`（**上界为 git repo root**，无 repo 到文件系统根；trust 门控；`~/.agents/skills` 从祖先扫描排除）、packages、settings `skills` 数组（glob/`!`/`+`/`-`）、CLI `--skill`
- **两种发现模式**：pi 目录（`~/.pir/agent/skills`、`.pir/skills`、settings 路径）根级散放 `.md` 也算 skill；`.agents/skills` 忽略根级散放 `.md`，只认 `SKILL.md` 目录
- 目录含 `SKILL.md` 即 skill root 不再递归；跳过 `.` 开头目录与 node_modules；遵守 `.gitignore`/`.ignore`/`.fdignore`；跟随符号链接
- frontmatter：代码读 `name`（≤64、`^[a-z0-9-]+$`、不连续连字符、不首尾连字符；违规仅警告仍加载）、`description`（≤1024，**缺失则不加载**）、`disable-model-invocation`；spec 其余字段（license/compatibility/metadata/allowed-tools）解析但忽略
- 渐进披露：system prompt 仅 `<available_skills>` XML（name/description/location），**且仅当 read 工具激活时注入**；`disable-model-invocation: true` 不进 prompt
- `/skill:name [args]`：展开为 `<skill name location>…body</skill>` + `References are relative to <baseDir>` 行，**args 原样追加在块后**（以代码为准；上游 skills.md 的 `User: <args>` 描述与代码不符）；`enableSkillCommands` 默认 true
- 同名冲突先到先得 + collision 诊断

### 7.3 Prompt Templates

- `*.md` → `/name`（文件名去 `.md`）；**非递归**、跟随符号链接
- 路径：`~/.pir/agent/prompts/`、`.pir/prompts/`（trust）、packages、settings、CLI `--prompt-template`
- frontmatter：`description`（缺省取正文首个非空行截 60 字符 + `...`）、`argument-hint`（`<>`/`[]` 语义）
- 参数展开 DSL（引号感知 bash 风格解析；替换不递归；缺位 `$N` 展开为空串）：`$1..$N`、`$@`、`$ARGUMENTS`、`${N:-default}`、`${@:-default}`、`${ARGUMENTS:-default}`、切片 `${@:N}`、`${@:N:L}`（1-indexed）

### 7.4 Themes

- 内置 dark/light；首次运行按终端背景自动选择；theme 设置值可为 `light/dark` 配对（parseAutoThemeSetting 拆分），终端配色检测链为 OSC 11 背景查询 → COLORFGBG → fallback，终端配色变化时动态切换
- 自定义 JSON schema：`name`、`vars`（变量引用表）、`colors`（**51 必填 + `thinkingMax` 可选**，缺省回退 `thinkingXhigh`）、`export`（pageBg/cardBg/infoBg，HTML 导出用）
- ColorValue 三形态：hex 字符串 / 0-255 整数（256 色）/ `""`（默认）
- 路径：`~/.pir/agent/themes/`、`.pir/themes/`（trust）、packages、settings、CLI `--theme`；settings `theme` 值中 `/` 是 auto light/dark 分隔符（如 `light/dark`，parseAutoThemeSetting 拆分），主题 name 正则 `^[^/]+$` 禁止含 `/`，**无「按路径处理」分支**
- 热重载：fs watcher **只 watch 全局 `~/.pir/agent/themes/<当前主题>.json`**，项目主题不 watch

### 7.5 Keybindings

- **仅全局** `~/.pir/agent/keybindings.json`（无项目级）；值为 string 或 string[]
- 命名空间 id：`tui.editor.*`/`tui.input.*`/`tui.select.*` + `app.*`；**旧键名自动迁移表 60+ 项**（保留，ADR-0003 §3）
- 默认表 = pi-tui `TUI_KEYBINDINGS` + 42 个 `app.*`；平台差异默认值（win32 无 ctrl+z suspend、粘贴图片 win32 为 alt+v、macOS tree 方向键顺序不同）
- 完整默认绑定表对齐 `docs/keybindings.md`（约 80 个动作，逐条对拍）；`/hotkeys` 查看；`/reload` 热应用
- **键位判断永不硬编码**（进默认值表）；例外：shift+ctrl+d（/debug）为硬编码全局键

### 7.6 Packages

- source 解析顺序：`npm:` 前缀 → 本地路径判定 → git URL（`git:` 简写两种；无前缀只认 `https?/ssh/git://`）→ 回退本地路径
- npm：`name@version`，精确版本=pinned（更新跳过），range 用 semver maxSatisfying；安装到 `~/.pir/agent/npm/` / `.pir/npm/`；`npmCommand` argv wrapper 时 git 包依赖安装退化裸 `install`
- git：克隆到 `~/.pir/agent/git/<host>/<path>` / `.pir/git/...`；**pinned ref 不移动但 update 会 reconcile 已有克隆到配置 ref**（reset+clean+必要时 install 依赖）；temporary scope 未 pin 且非离线时启动刷新
- `-e` 临时加载：npm/git 装到 `~/.pir/agent/tmp/extensions`（0700）
- settings `packages`：string 或 object（`autoload:false` 时对 user 同名包做 delta）；过滤语法 glob / `!`排除 / `+`强制含 / `-`强制排除（SKILL.md 可按父目录名匹配）
- 身份去重：npm 按包名、git 按 host/path（无 ref URL）、local 按绝对路径；project 覆盖 user
- 资源优先级 rank：project settings > project auto > user settings > user auto > package
- 离线跳过安装/更新；网络超时 10s；更新检查并发 4
- 核心包须列 `peerDependencies:"*"`（声明式资源包布局对齐）

> 实现注记（T14-W2，详见偏离 D-040）：落地于 `crates/pir/src/core/package_manager.rs` +
> `core/git_url.rs` + `cli/package_command.rs`。hosted-git-info 以五 host 子集自实现；
> npm semver 用 `semver` crate + range 翻译层（`||`/`x` 通配/部分版本/完整版连字符范围，
> prerelease range 等极端形式视为无效 range 回退）；glob 复用内置 matcher（`{a,b}`/`[abc]`
> 不支持，同 D-014/D-039）；npm/git 进程经 `PackageCommandRunner` 注入；`resolve()` 的包切片
> 输出接 `resource_loader` 的 `package_resources` 端口（会话启动接线在后续波次）；
> 上游 `list` 接受并忽略位置参数的 quirk 保留。
>
> 实现注记（T14-W3，详见偏离 D-041）：`update` 编排（`update` / `checkForAvailableUpdates` /
> 并发 4 worker pool）、版本检查（`core/version_check.rs`，HTTP 经 `LatestVersionTransport`
> 注入，endpoint 集中常量 `LATEST_VERSION_URL` 留 W6 配置化口子）与自更新
>（`config.rs` self-update 段：install-method 检测、各包管理器命令构造、可写性检查）落地；
> pir 恒为原生二进制，非包管理器安装按上游 bun-binary 结局打印 releases 页提示
>（`SELF_UPDATE_DOWNLOAD_URL` 常量）；release note 经 pir-tui Markdown identity 主题渲染。

### 7.7 Settings

完整对齐 `docs/settings.md`（全局 `~/.pir/agent/settings.json` + 项目 `.pir/settings.json`）。全键清单（默认值）：

`lastChangelogVersion`、`defaultProvider`、`defaultModel`、`defaultThinkingLevel`(off)、`transport`(auto)、`steeringMode`/`followUpMode`(one-at-a-time)、`theme`、`compaction.{enabled:true,reserveTokens:16384,keepRecentTokens:20000}`、`branchSummary.{reserveTokens:16384,skipPrompt:false}`、`retry.{enabled:true,maxRetries:3,baseDelayMs:2000,provider.{timeoutMs,maxRetries:0,maxRetryDelayMs:60000}}`、`hideThinkingBlock`(false)、`showCacheMissNotices`(false)、`externalEditor`（>VISUAL>EDITOR>notepad/nano）、`shellPath`、`shellCommandPrefix`、`quietStartup`(false)、`defaultProjectTrust`(ask，仅全局)、`npmCommand`、`collapseChangelog`(false)、`enableInstallTelemetry`(true)、`enableAnalytics`(false)、`trackingId`（opt-in 生成 UUID）、`packages`、`extensions`、`skills`、`prompts`、`themes`、`enableSkillCommands`(true)、`terminal.{showImages:true,imageWidthCells:60,clearOnShrink:false,showTerminalProgress:false}`、`images.{autoResize:true,blockImages:false}`、`enabledModels`、`doubleEscapeAction`(tree)、`treeFilterMode`、`thinkingBudgets.{minimal,low,medium,high}`、`editorPaddingX`(0，0-3)、`outputPad`(1)、`autocompleteMaxVisible`(5，3-20)、`showHardwareCursor`、`markdown.codeBlockIndent`("  ")、`warnings.anthropicExtraUsage`(true)、`sessionDir`、`httpProxy`（仅全局）、`httpIdleTimeoutMs`(300000)、`websocketConnectTimeoutMs`(15000)。

合并语义：项目覆盖全局，嵌套对象仅**单层浅合并**（深度≥2 的嵌套对象整体替换；上游 `deepMergeSettings` 注释自称递归是错的）、**数组与原始值整体替换**；字段级写持久化（只写 session 内改过的字段，嵌套按键合并）+ proper-lockfile 等价物；旧格式迁移（queueMode→steeringMode、websockets bool→transport、旧 skills 对象格式、retry.maxDelayMs→provider.maxRetryDelayMs）；trust=false 时项目 settings 视为空且拒写；parse 错误按 scope 记录并阻止覆写。

### 7.8 Project Trust

- `trust.json`（agentDir）：`{规范化绝对路径: true|false}`，null 删除；按 key 排序写盘 + 目录级 lockfile
- 决策查找沿**父目录链向上取最近条目**
- 触发条件 `hasTrustRequiringProjectResources`：`.pir/` 下存在 settings.json/extensions/skills/prompts/themes/SYSTEM.md/APPEND_SYSTEM.md 任一，或 cwd/祖先存在 `.agents/skills`（`~/.agents/skills` 豁免）；裸 `.pir/` 目录不算；无此类资源直接 trusted
- 解析优先级链：`--approve`/`--no-approve` override → 扩展 `project_trust` 事件（yes/no/undecided，首个 yes/no 胜，`remember:true` 持久化）→ trust.json → `defaultProjectTrust`（always→true、never→false、ask→有 UI 弹窗、**无 UI 返回 false**）
- 交互弹窗 5 选项：Trust / Trust parent folder (path) / Trust (this session only) / Do not trust / Do not trust (this session only)
- **两阶段加载**：信任前仅 context 文件 + 全局/CLI/inline 扩展（可处理 `project_trust` 事件，handler ctx 为受限子集）；信任后 setProjectTrusted 并完整 reload
- `/trust` 只写 trust.json、不重载当前会话；`pir config`/包管理命令同流程；`pir update` 永不提示

> 实现注记（T14-W4，D-043）：决策链落 `core/trust_manager.rs::resolve_project_trusted`（同步；扩展事件经 `extension_event` 参数预发射，`ExtensionRunner::emit_project_trust` 默认 None 待 T15 接通）；启动弹窗落 `modes/interactive/startup_ui.rs::run_startup_selector`（复用 ExtensionSelectorComponent，hasUI=初始 runtime 且 interactive 且非 help/--list-models）；两阶段接线在 `app.rs` create_runtime（未信任建 services → 判定 → set_project_trusted + reload），差集锚点 `tests/resource_loader_test.rs::set_project_trusted_loads_second_phase_resources`。遗留：交互模式 resume 到异 cwd 的信任提示（上游 switchSession projectTrustContextFactory）未接线，`CreateRuntimeOptions.project_trust_context` 已留口子。

---

## 8. Interactive UX 需求

### 8.1 布局

Startup header → messages → editor → footer。

- **Footer 行 1**：`cwd（home 缩写 ~） (git branch) • session name`
- **Footer 行 2**（左）：`↑input ↓output R<cacheRead> W<cacheWrite> CH<命中率>% $cost[ (sub)] <context%>/<window>[ (auto)] [• xp]`；context >70% 黄、>90% 红；（右）`[(provider)] model[ • thinking level]`；宽度不足逐级截断；第三行扩展 status（按 key 字母序单行截断）
- **Header**：logo `pir v<version>` + 快捷键提示（紧凑/展开两态，**随 Ctrl+O 与工具展开联动**）+ onboarding 行 + changelog；`quietStartup` 时为空；扩展可 `setHeader` 整体替换

### 8.2 Editor

- 多行、undo（**Ctrl+-**，快照含 paste registry，连续词符合并一个 undo unit）、kill-ring（Ctrl+Y / Alt+Y yankPop）
- **历史**：up/down 导航，100 条上限，进入时保存草稿
- bracketed paste → 大粘贴 marker（**>10 行或 >1000 字符** → `[paste #N +X lines]` / `[paste #N X chars]`，marker 为原子 segment 参与光标/删除/折行）；tmux csi-u 重编码修正；粘贴路径前自动补空格
- `@` 文件模糊搜索（fd 行为等价，ADR-0003 §2 原生实现）、引号路径 `@"..."` 含空格、`~/` 展开；Tab 补全上下文分派（slash context → 命令；否则强制文件补全）
- Shift+Enter / Ctrl+J 换行；Ctrl+G 外置编辑器（临时 `prompt.md`，退出码非 0 失败，Windows shell spawn）
- Ctrl+V 图文粘贴（**Windows 为 Alt+V**；图片写临时文件后插入路径，文本 fallback）；拖拽文件路径 attach
- `!`/`!!` bash（输入 `!` 开头时 editor 边框变色）
- autocomplete：四类命令源合并（builtin/extension/prompt/skill）；/model、/login 参数级 fuzzy 补全；防抖双档（Tab 0ms、attachment 有防抖）；`autocompleteMaxVisible` 5（3-20）；扩展可注入 trigger characters 与 provider
- 扩展可整体替换 editor（`setEditorComponent`）

### 8.3 消息队列

- streaming 中 Enter → steering；Alt+Enter → follow-up（**非 streaming 时 Alt+Enter == Enter**）
- compaction 期间独立第二队列 `compactionQueuedMessages`；扩展命令立即执行不入队
- Escape abort 并恢复队列到编辑器；Escape 优先级链：streaming abort → bash abort → 退出 bash mode → 空 editor 双击手势
- Alt+Up：**全部**队列消息（steering+followUp 两队）以 `\n\n` 拼接后与当前 editor 文本合并放回（一次性，非逐条）
- 队列显示于 pendingMessages 容器，附 dequeue 提示；消化模式见 `steeringMode`/`followUpMode` 设置

### 8.4 Slash 命令

**四类来源**：builtin（内置优先，同名冲突告警）/ extension `registerCommand` / prompt template / skill（`/skill:<name>`）。

**内置 22 个**：`/settings` `/model` `/scoped-models` `/export` `/import` `/share` `/copy` `/name` `/session` `/changelog` `/hotkeys` `/fork` `/clone` `/tree` `/trust` `/login` `/logout` `/new` `/compact` `/resume` `/reload` `/quit`。

**隐藏/特殊**：`/debug`（写 debug log 到文件，无 autocomplete）；`/llama`（**内置 hidden 扩展注册**，非内置命令）；彩蛋 `/arminsayshi` `/dementedelves`（可 [DEFER]）。

> 实现注记（T14-W6b，详见偏离 D-047）：llama.cpp 集成落 `crates/pir/src/extensions/llama/`（client/huggingface/provider/编排流）+ TUI `modes/interactive/components/llama_view.rs`。T15 宿主缺位期的等价接线：命令表 `extensions/mod.rs::BUILT_IN_EXTENSION_COMMANDS` + dispatch fall-through + autocomplete 直供；provider 进程级单例于 `create_agent_session_services` 注册（`register_native_provider`）。`/login` 的 api-key 通路（含 bare `/login` 的 auth-type 预选择器、`/login <provider>` 精确匹配直进、`Models/ModelRuntime::login/logout`）同波次接线；OAuth 对话框流仍为 stub（T13 遗留）。

**带参数形式**：`/model <模糊词>`（provider/id fuzzy）、`/export <path.html|jsonl>`、`/import <file.jsonl>`、`/name <名字>`、`/compact <自定义指令>`、`/login <provider>`（fuzzy）。

### 8.5 快捷键

完整默认表对齐 `docs/keybindings.md`（约 80 动作，逐条对拍）。要点：

- 编辑器：光标（up/down/left/right、ctrl+b/f、alt+left/right、ctrl+left/right、alt+b/f、home/end、ctrl+a/e、**ctrl+]/ctrl+alt+] jump 到字符**、pageUp/pageDown）；删除（backspace、delete/ctrl+d、ctrl+w/alt+backspace、alt+d/alt+delete、ctrl+u、ctrl+k）；shift+enter/ctrl+j 换行；enter 提交；tab 补全
- App 级：escape（interrupt）、ctrl+c（clear，**双击 500ms 退出**）、ctrl+d（空 editor 退出）、ctrl+z（suspend，Windows 无默认绑定）、shift+tab（cycle thinking）、ctrl+p / shift+ctrl+p（模型正/反 cycle）、ctrl+l（**打开 model selector，非清屏**）、ctrl+o（tools expand + header 帮助联动）、ctrl+t（thinking blocks 显隐）、ctrl+n（named session filter）、ctrl+g（外置编辑器）、ctrl+x（复制消息）、alt+enter（follow-up）、alt+up（dequeue）、ctrl+v 贴图（win32 alt+v）
- 双 Escape（500ms）可配置动作：`doubleEscapeAction` = tree/fork/none
- 无默认绑定：app.session.new/tree/fork/resume
- 局部键：session selector（ctrl+p 路径显隐、ctrl+s 排序、ctrl+r 重命名、ctrl+d 删除、ctrl+backspace）；scoped-models（ctrl+s/a/x/p、alt+up/down）；tree（ctrl+left/right 或 alt+left/right fold、shift+l/shift+t label、ctrl+d/t/u/l/a filter、ctrl+o cycle）
- 硬编码全局键：**shift+ctrl+d** = /debug

### 8.6 TUI 引擎

- **渲染**：CSI 2026 synchronized output 包裹；首次全量（不清屏）/ 全量清屏（`\x1b[2J\x1b[H\x1b[3J`）/ 行差分（含 append 快路径、纯删除快路径、无变化只移硬件光标）；**全量回退条件**：宽度变化、高度变化（**Termux 例外**）、clearOnShrink 收缩、`firstChanged < viewportTop`、删除行数超终端高度、`requestRender(force)`；16ms 节流；viewport 概念；行尾 SGR + OSC 8 reset；Kitty 图像差分区间扩展 + 先删旧图像
- **输入**：stdin 分块重组（CSI/OSC/DCS/APC/鼠标跨 chunk）；bracketed paste 缓冲；Kitty keyboard（flags=7，**含 key release/repeat**，组件可声明 `wantsKeyRelease`）+ legacy 全表；DA 探测无 Kitty 应答立即回退 modifyOtherKeys（无超时等待）；退出前 drainInput 防序列泄漏
- **Overlay/Focus/IME**：overlay 栈合成后差分；9 种 anchor + offset/百分比/min/max/margin/`visible()`；OverlayHandle（focus/unfocus/setHidden/hide）；focus 恢复状态机；`CURSOR_MARKER` 零宽 APC 序列定位硬件光标（默认隐藏，`showHardwareCursor`/`PIR_HARDWARE_CURSOR=1`）；容器组件传播 focused
- **组件（pi-tui 12 个）**：Text、Box、Container、Spacer、Markdown、Image、SelectList、Input、Editor、Loader、CancellableLoader、TruncatedText、SettingsList
- **coding-agent 交互组件 40 个**：assistant-message、user-message、tool-execution、diff、bash-execution、branch-summary-message、compaction-summary-message、skill-invocation-message、custom-message、custom-entry、footer、custom-editor、extension-editor、extension-input、model-selector、scoped-models-selector、settings-selector、theme-selector、thinking-selector、login-dialog、oauth-selector、config-selector、session-selector(+search)、tree-selector、user-message-selector、trust-selector、extension-selector、show-images-selector、first-time-setup、bordered-loader、countdown-timer、status-indicator、keybinding-hints、dynamic-border、visual-truncate 等（彩蛋组件可 [DEFER]）
- **Markdown**：marked 等价解析（Rust 落地为 **comrak 0.54** 替代 marked@18.0.5，AST 对应与 2 条残留边缘差异见偏离 D-018）；`trimPartialClosingFences()` 流式 fence 稳定（防代码块闪烁）；code block border + indent；主题 20+ 样式函数；语法高亮
- **Image**：Kitty（`\x1b_G`，图像 ID 分配/删除/占位行）+ iTerm2（`\x1b]1337;File=`）；能力检测：kitty/ghostty/wezterm/warp → kitty 协议；iTerm2 → iterm2；**tmux/screen → 禁用图像**（tmux 探测 OSC 8 转发）；Windows Terminal/VSCode/Alacritty → 无图像有 hyperlink；JetBrains → 无 hyperlink；未知终端保守回退 `text (url)`；不支持时 `imageFallback`
- **终端特例**：Windows Terminal（Ctrl+Backspace 启发、VT input 开启）、tmux（modifyOtherKeys 兼容、图像禁用）、Apple Terminal（Shift+Enter 归一化、原生修饰键检测）、Termux（高度变化不全量重绘）、Ghostty（`shift+enter=\n`）、WezTerm（kitty_keyboard Escape 特例）、screen/VSCode/Alacritty/JetBrains（能力层）（Rust 落地注记：Windows VT input（Shift+Tab 归一化）与 Apple Terminal Shift+Enter 归一化所依赖的**原生修饰键检测**为已知平台缺口——pir 无原生绑定，依 [ADR-0004](./adr/0004-platform-native-helper-gaps.md) 恒定走上游 addon 缺失回退路径；macOS / Windows 上对应键位与上游（有绑定时）不一致，Linux 行为与上游一致；见偏离 D-016）
- **终端自省四件套**：OSC 11 背景色查询（`\x1b]11;?\x07`）、CSI ?996n 配色模式查询（`\x1b[?996n`）、CSI 16t 像元查询（`\x1b[16t`）、OSC 9;4 任务栏进度上报（`\x1b]9;4;3\x07` indeterminate / `\x1b]9;4;0;\x07` clear，含 1s keepalive `TERMINAL_PROGRESS_KEEPALIVE_MS=1000`；对应 `terminal.showTerminalProgress` 设置）

---

## 9. 扩展系统需求

### 9.1 能力清单（API 形状 1:1）

**加载**：jiti 等价机制不复制（TS 加载不做）；发现路径 `~/.pir/agent/extensions` 与 `.pir/extensions`（一层：散文件 + 子目录 index/manifest）、packages、CLI `-e`、inline factory（可 named/hidden）；模块缓存按 cwd+generation；同名冲突分项规则——工具/flag 首注册胜 + 有诊断；renderer 首注册胜且静默；命令（command）全部保留、重名加 `:N` 后缀（invocationName = `name:occurrence`），扩展间重名无诊断（仅与内置命令冲突时有诊断）；shortcut 为 last-wins + 诊断；`/reload`；内置 hidden llama.cpp 扩展。

**事件全集 33 个**：`project_trust`、`resources_discover`（可补充 skill/prompt/theme 路径，reason: startup|reload）、`session_start`、`session_info_changed`、`session_before_switch`、`session_before_fork`、`session_before_compact`、`session_compact`、`session_shutdown`、`session_before_tree`、`session_tree`、`context`、`before_provider_request`、`before_provider_headers`、`after_provider_response`、`before_agent_start`、`agent_start`、`agent_end`、`agent_settled`、`turn_start`、`turn_end`、`message_start`、`message_update`、`message_end`、`tool_execution_start`、`tool_execution_update`、`tool_execution_end`、`model_select`、`thinking_level_select`、`user_bash`、`input`、`tool_call`、`tool_result`（对齐 `extensions.md` 生命周期图逐序列对拍）。

**事件可变语义**：`tool_call` 原地改参或 block+reason；`tool_result` 改 content/details/isError/usage；`input` 三态（continue/transform/handled）；`user_bash` 换 operations 或整替 result；`before_agent_start` 注入 custom message + 链式替换 systemPrompt；`session_before_*` 字段级语义——`session_before_compact` 返回 {cancel?, compaction?}，compaction 提供完整 CompactionResult（summary/firstKeptEntryId/tokensBefore/estimatedTokensAfter?/usage?/details?）即整体接管并打 fromHook 标记；`session_before_tree` 返回 {cancel?, summary?, customInstructions?, replaceInstructions?, label?}，summary 仅在 summarize 模式下采用，后三者覆盖 preparation 同名字段；`session_before_fork` 声明 {cancel?, skipConversationRestore?} 但上游仅 cancel 生效（skipConversationRestore 为 reserved 字段）——pir 只实现 cancel 并登记该差异；`before_provider_request`（coding-agent 扩展层）handler 返回值**整体替换**请求 payload（unknown，非合并），多 handler 按注册顺序链式传递，返回 undefined 表示不替换；`before_provider_headers` handler 原地 mutate headers（Record<string, string|null>），返回值被忽略，值设为 null 删除该 header，在 attribution header 合并之后执行；`message_end` 可替换消息（保 role）；`context` 可替换 messages。

**API 方法全集（24 方法 + `events` 属性）**：`on()` + `registerTool` / `registerCommand` / `registerShortcut`（与内置键冲突按 `restrictOverride`）/ `registerFlag` + `getFlag` / `registerMessageRenderer` / `registerEntryRenderer` / `sendMessage`（deliverAs steer|followUp|nextTurn、triggerTurn）/ `sendUserMessage` / `appendEntry` / `setSessionName` / `getSessionName` / `setLabel` / `exec` / `getActiveTools` / `getAllTools` / `setActiveTools` / `getCommands` / `setModel` / `getThinkingLevel` / `setThinkingLevel` / `registerProvider`（双签名）/ `unregisterProvider` / `events`（共享 EventBus）。

**UI 方法全集（ExtensionUIContext）**：`select` / `confirm` / `input` / `notify` / `onTerminalInput` / `setStatus` / `setWorkingMessage` / `setWorkingVisible` / `setWorkingIndicator` / `setHiddenThinkingLabel` / `setWidget`（aboveEditor|belowEditor）/ `setFooter`（FooterDataProvider）/ `setHeader` / `setTitle` / `custom`（overlay + OverlayHandle）/ `pasteToEditor` / `setEditorText` / `getEditorText` / `editor` / `addAutocompleteProvider` / `setEditorComponent` / `getEditorComponent` / `theme` / `getAllThemes` / `getTheme` / `setTheme` / `getToolsExpanded` / `setToolsExpanded`。

**Context（三级层级：`ExtensionContext` ← `ExtensionCommandContext` ← `ReplacedSessionContext`）**：`isIdle` / `isProjectTrusted` / `signal` / `abort` / `hasPendingMessages` / `shutdown` / `getContextUsage` / `compact` / `getSystemPrompt`；CommandContext 追加 `getSystemPromptOptions` / `waitForIdle` / `newSession` / `fork` / `navigateTree` / `switchSession` / `reload`（session 替换后旧 ctx 作废，`withSession` 拿新 ctx）；第三级 `ReplacedSessionContext` 为 session 替换后绑定新 session 的 command-capable ctx（经 `newSession`/`fork`/`switchSession` 的 `withSession` 回调传入），追加 `sendMessage` / `sendUserMessage`。

**动态工具**：工具执行期间 `setActiveTools` 新增的工具经结果的 `addedToolNames` 暴露（含模型原生 deferred loading 回退）。

**模式差异**：tui 全能力；rpc `hasUI=true` 对话框协议化、`custom()` 返回 undefined（完整降级清单见 §2.4）；print/json `hasUI=false`、UI 全 no-op。

### 9.2 实现范围（已决策）

| 级别 | 需求 ID | 状态 |
|------|---------|------|
| L0 | EXT-RUST | **必做**：Rust trait 同构 API，内置扩展 + 动态库插件 |
| L1 | EXT-WASM | **必做**：Wasm 插件 + 与 L0 同一能力面的 host ABI |
| L2 | EXT-TS | **不做**：嵌入 JS / 跑现有 `.ts` 扩展 |

扩展需用 Rust/Wasm 重写。**安装与包管理列入正式计划**（本地路径 + 可分发 Wasm 包；`install`/`remove`/`list`/`update`/`config`）。Skills / prompts / themes 声明式资源格式仍与 Pi 对齐。Wasm runtime **嵌入主二进制**。见 ADR-0001 / ADR-0002。

注意：ExtensionAPI 表面为 33 事件 + 24 API 方法（+ `events` 属性）+ 28 UI 方法 + 三级 Context；其中 UI 的组件工厂类方法（setWidget/setFooter/setHeader/custom/setEditorComponent）携带 TUI Component 类型，Rust/Wasm 化需要额外协议设计（M0 spike 验证，见 `02-design.md` §7）。

---

## 10. 安全、运维与分发

- 扩展/包/skills 以用户权限运行；文档警告对齐（`docs/security.md`）
- Containerization 文档三种模式可移植说明（Gondolin micro-VM 扩展 / 纯 Docker / OpenShell）
- **单文件部署**：发布单一可执行文件；Wasm 扩展 runtime 打进主包
- 版本检查 / telemetry：**支持配置自有 endpoint**（settings / `PIR_*`，默认 `pi.dev` 可改）；可关闭；`enableInstallTelemetry`/`enableAnalytics`（opt-in）
- `/debug`（及 shift+ctrl+d）写调试日志：最近渲染行（ANSI）+ 最近发给 LLM 的消息
- Provider payload debug：对齐 Pi 的调试开关（`onPayload`/`onResponse`、Codex WS debug stats）

## 10.1 许可证

**MIT**（与 Pi 相同）。

---

## 11. 质量需求

### 11.1 测试

- 单元：agent loop 事件序、工具序、session 迁移、compaction 切点、edit fuzzy 匹配、prompt template 展开、settings 合并
- 契约：RPC 32 命令 + 扩展 UI 子协议 / session JSONL schema / 黄金 JSONL
- Provider：faux + 可选集成测试（有 key 时，`PIR_LIVE_TEST=1`）
- TUI：
  - 关键 ANSI 序列子集 diff（去 CSI 2026 同步输出抖动后，与 Pi 虚拟终端输出比对）
  - 组件级渲染快照黄金文件（Editor / SelectList / Markdown / SettingsList 等）
  - 真机矩阵（Kitty / Ghostty / Windows Terminal / tmux 等）仅 smoke 验收
- 回归：移植 Pi coding-agent regression 用例意图
- **逐条对拍级基准**：`session-format.md`、`rpc.md`、`compaction.md`、`keybindings.md`、`tmux.md`/`terminal-setup.md`（字节序列）、`docs/keybindings.md` 默认绑定表

### 11.2 性能

- 流式首 token 延迟相对 Node 版劣化 ≤ 20%（同网络、同 provider、各 3 次取中位数）
- TUI 渲染节流 ~16ms；大 session 滚动可用
- 二进制体积：< 50MB 压缩前（含嵌入的 wasmtime）；M0 ABI spike 时实测验证

### 11.3 兼容性

- Linux（主）、macOS、Windows
- 终端：Kitty、Ghostty、Alacritty、iTerm2、Windows Terminal、tmux、VS Code terminal

---

## 12. 文档需求

- 用户文档结构镜像 `coding-agent/docs/`
- 开发者：crate 边界、扩展 ABI、对拍测试说明
- CHANGELOG 跟踪相对上游的差异

---

## 13. 里程碑验收（需求视角）

| 里程碑（对齐设计文档 M0–M8） | 必须满足的需求章节 |
|--------|-------------------|
| M0 骨架与对拍基建 | §11.1（对拍 harness、fixtures、归一化 diff）；Wasm ABI spike |
| M1–M2 Headless 核心 | §4.1–§4.3、§4.5、§5（≥2 协议）、默认四工具 |
| M3–M4 Headless MVP | §4.4（harness）、§6（含 §6.6）、§2.2–2.4（RPC 32 命令）、token 估算对拍 |
| M5 Interactive | §8、§3 主命令 |
| M6 Providers 全量 | §5 全量（38 providers + 7 OAuth + compat 矩阵） |
| M7 资源与包管理 | §7（含 packages / trust / export / llama.cpp）、§4.5（可选工具 grep/find/ls） |
| M8 扩展 + Parity Freeze | §9（L0+L1 与安装）、全文档对拍清单、session 互通 |

---

## 14. 需求追溯

| 需求域 | Pi 文档 / 源码 |
|--------|----------------|
| 总览 / CLI / 日常行为 | `packages/coding-agent/README.md`、`docs/usage.md`、`docs/quickstart.md`、`src/cli/args.ts`、`src/main.ts` |
| 会话行为 | `docs/sessions.md`、`src/core/agent-session.ts` |
| Session 格式 | `docs/session-format.md`、`src/core/session-manager.ts` |
| Compaction | `docs/compaction.md`、`src/core/compaction/*` |
| Extensions | `docs/extensions.md`、`src/core/extensions/*` |
| RPC/JSON | `docs/rpc.md`、`docs/json.md`、`src/modes/rpc/*` |
| SDK | `docs/sdk.md` |
| Settings/环境变量/Trust | `docs/settings.md`、`docs/environment-variables.md`、`docs/security.md`、`src/core/settings-manager.ts`、`src/core/trust-manager.ts` |
| Keybindings | `docs/keybindings.md`、`src/core/keybindings.ts`、`packages/tui/src/keybindings.ts` |
| Skills/Prompts/Themes/Packages | `docs/skills.md`、`docs/prompt-templates.md`、`docs/themes.md`、`docs/packages.md` |
| Providers/Auth/models.json | `docs/providers.md`、`docs/models.md`、`docs/custom-provider.md`、`packages/ai/src/auth/*` |
| llama.cpp | `docs/llama-cpp.md`、`src/extensions/llama/*` |
| Agent loop / harness | `packages/agent/README.md`、`agent-loop.ts`、`agent.ts`、`harness/*` |
| AI | `packages/ai/README.md`、`packages/ai/src/api/*` |
| TUI | `packages/tui/README.md`、`docs/tui.md`、`packages/tui/src/*` |
| 平台 | `docs/windows.md`、`docs/termux.md`、`docs/tmux.md`、`docs/terminal-setup.md`、`docs/shell-aliases.md`、`docs/containerization.md` |

---

## 15. 范围排除（ADR-0003）

以下上游内容**不在复刻范围**，文档化以免争议：

- `packages/server`：实验性 pi 实例管理器（Unix socket IPC 监督 RPC 进程），coding-agent 不依赖它，`/share` 与其无关
- `packages/evals`：private 行为评估包（其 `pi-harness.ts` 可作为对拍 harness 参考实现）
- `coding-agent/src/bun`：Bun 单文件打包入口（Rust 原生单二进制无对应物；其 `restore-sandbox-env` 提示沙箱下环境变量可用性验收点）
- `packages/storage/sqlite-node`：harness 可选 SQLite 后端（仅 JSONL，ADR-0002 §7）
- pi-ai 包级 CLI（`login` 写 CWD/auth.json）：凭据统一走 `pir` 主 CLI（ADR-0003 §4）
- Pi migrations.ts 的 legacy 启动迁移（ADR-0003 §3；keybindings 旧键名迁移表除外）
- 兼容现有 TS / jiti pi-package 扩展（ADR-0001）
