# T10：Headless 模式 — print / json / rpc

- **状态**：已完成
- **里程碑**：M4
- **依赖**：T03、T04、T05、T06、T07、T08、T09
- **上游对照**：`packages/coding-agent/src/cli/*`、`src/main.ts`、`src/modes/*`、`src/rpc-entry.ts`、`docs/rpc.md`（逐条对拍级基准）、`docs/json.md`、`docs/sdk.md`、`docs/usage.md`
- **需求章节**：§2.2–§2.5、§3.1–§3.3
- **预估**：1.5–2 人月

---

## 目标

打通启动管线与三种 headless 运行模式，交付可脚本化使用的 `rpi`：
print 打印最终文本、json 输出事件流、rpc 提供 32 命令环，同时沉淀 Rust SDK 表面。

## 范围

### In

- CLI 解析（clap）：需求 §3.1 主命令标志全集**含精确语义**：
  - `@file` 位置参数（文本 `<file name>` 标签 / 图片 ImageContent resize 2000×2000 / 空文件跳过 / 不存在 exit 1；RPC 模式禁止）
  - `-p` 值吞噬规则；`--model provider/pattern:thinking` 解析；`--models` glob/模糊/`:thinking`；`--api-key` 必须搭配 model（非持久 override）
  - `--session` 三级解析（路径 → 本项目 id → 全局跨项目 + 交互确认 fork）；`--session-id` 正则校验与「不存在则新建」；`--fork`/`--session-id` 互斥矩阵
  - **未知 `--flag` 收集为扩展标志**（`extensionFlagValues` 透传，help 动态段）；单 `-x` 未知为 error diagnostic
  - diagnostics 体系（warning/error，error exit 1；非法 thinking level 仅 warning）
- 启动管线（设计文档 §6.1）：`--offline` → 同时设 `RPI_SKIP_VERSION_CHECK`；模式解析 rpc > json > print（-p 或**非 TTY 自动**，含 interactive + piped stdin 降级）→ SettingsManager → trust gate（非交互不提示）→ services → SessionManager（含 header cwd 缺失处理）→ AgentSession → mode 分发；**不实现** migrations.ts（ADR-0003 §3）
- `AgentSession` / `AgentSessionRuntime`：prompt / steer / follow_up / abort、compaction 接入（双路触发）、事件映射 `AgentSessionEvent`（全集，需求 §2.3）、JSONL 持久化接线（延迟落盘）
- print 模式：初始 prompt（含 piped stdin 合并）→ **依次发送全部消息** → 打印最后 assistant **text 块** → 退出；**error/aborted → stderr + exit 1**；SIGTERM/SIGHUP → 143/129
- json 模式：**原样 session header 行** + `AgentSessionEvent` JSONL 单向流
- rpc 模式：
  - **严格 LF** JSONL 帧（自实现行读取，不按 U+2028/U+2029 拆分；容忍行尾 `\r`）
  - **命令全集 32 个**（需求 §2.4 清单，逐条）：prompt / steer / follow_up / abort / new_session / get_state / get_messages / set_model / cycle_model / get_available_models / set_thinking_level / cycle_thinking_level / get_available_thinking_levels / set_steering_mode / set_follow_up_mode / compact / set_auto_compaction / set_auto_retry / abort_retry / **bash / abort_bash**（经 T06 bash_executor，excludeFromContext、`bash_execution_update` 带 id）/ get_session_stats / export_html / switch_session / fork / clone / get_fork_messages / get_entries / get_tree / get_last_assistant_text / set_session_name / get_commands
  - prompt 响应异步化（preflight 后发出）；解析失败 `command:"parse"` 错误
  - 关闭语义：stdin EOF 退；`ctx.shutdown()` 等 `agent_settled`；SIGTERM=143/SIGHUP=129
  - session 替换（new/fork/switch/clone）后 rebind 扩展与事件订阅
  - 扩展 UI 协议层预留（9 方法 + 降级清单；T15 接线）
  - 独立入口 `rpi-rpc` bin（等价 `--mode rpc`）
- Rust SDK 表面：`create_agent_session` / `create_agent_session_runtime` / `SessionManager` / `ModelRuntime` / `ResourceLoader` 公开 API

### Out

- RPC 扩展 UI 往返（T15 扩展宿主就绪后补齐；本任务协议层预留）
- interactive 模式（T12）
- `--export` HTML 实现（T14；本任务留参数占位与「导出后退出」路径）
- 首次运行 setup（主题选择 + analytics opt-in，T12 交互面）

## 开发要点

- RPC 与后续 Interactive 共享 `AgentSessionRuntime`，避免两套会话逻辑（设计文档 §6.6）
- `/new`、切 cwd、resume 时重建 cwd 绑定服务（设计文档 §6.1）
- 三模式的对拍场景走 T02 fixtures：单轮问答、read/bash 工具、steering/follow-up、abort、length 截断、compaction
- RPC 命令面对照 `docs/rpc.md` 建逐条核对清单（G3 逐条对拍级基准）

## 设计细化（2026-08-04）

### 模块映射

| 上游（pi 0.82.1 @ 2efa728） | Rust 落地 | 说明 |
|---|---|---|
| `cli/args.ts` | `rpi/src/cli/args.rs` | **手写解析器**（与上游同构；clap 无法表达 -p 吞噬、未知 `--flag` 收集、互斥诊断矩阵，见偏离登记） |
| `cli/args.ts` diagnostics | `rpi/src/cli/diagnostics.rs` | `Diagnostic{level, scope, message}`，error exit 1 |
| `cli/file-processor.ts` | `rpi/src/cli/file_processor.rs` | `@file` 文本/图片处理 |
| `core/agent-session.ts` | `rpi/src/core/agent_session.rs` | AgentSession + AgentSessionEvent 全集（需求 §2.3） |
| `core/agent-session-runtime.ts` | `rpi/src/core/agent_session_runtime.rs` | 会话替换（new/fork/switch/import）+ rebind |
| `core/agent-session-services.ts` | `rpi/src/core/agent_session_services.rs` | cwd 绑定服务 + 扩展标志应用 |
| `core/model-runtime.ts` | `rpi/src/core/model_runtime.rs` | ModelRuntime（auth.json + models.json + stream_simple） |
| `core/model-resolver.ts` | `rpi/src/core/model_resolver.rs` | findInitialModel / `--model` / `--models` scope 解析 |
| `core/session-cwd.ts` | `rpi/src/core/session_cwd.rs` | header cwd 缺失（非交互直接 error） |
| `core/usage-totals.ts` | `rpi/src/core/usage_totals.rs` | SessionStats 聚合 |
| `core/extensions/*`（seam） | `rpi/src/core/extensions.rs` | **ExtensionRunner no-op seam**（真实宿主 T15）；RPC UI 9 方法 + 降级清单协议层预留 |
| `modes/print-mode.ts` | `rpi/src/modes/print_mode.rs` | |
| `main.ts` json 段 + `docs/json.md` | `rpi/src/modes/json_mode.rs` | |
| `modes/rpc/{rpc-mode,rpc-types,jsonl}.ts` + `docs/rpc.md` | `rpi/src/modes/rpc.rs` | 32 命令逐条契约 |
| `main.ts` 启动管线 | `rpi/src/app.rs` + `main.rs` | 模式分发 rpc>json>print(-p/非 TTY)；子命令分流留 T14 占位 |
| `rpc-entry.ts` | `rpi/src/bin/rpi_rpc.rs`（`[[bin]] rpi-rpc`） | 等价 `--mode rpc` |
| `sdk.ts` | `rpi/src/sdk.rs` + lib.rs re-export | create_agent_session / create_agent_session_runtime |

### 关键结构决策

1. **事件模型**：`Agent`（rpi-agent，Arc 共享）listener 为 async 且按序 await —— AgentSession 内部 listener 做持久化（message_end 先写 session 再转发听众），与上游 `_handleAgentEvent` 对齐；`AgentSessionEvent` 听众为同步回调 Vec（同上游 `_emit`）。
2. **Compaction**：复用 T08 `CompactionRunner`（持有 SessionManager）。T10 接线补丁：`model` 改 `Option<Model>`（无模型时 compact 报 no-model、check_compaction 直接 false，行为等价上游 `_runAutoCompaction` 的 `!this.model` 早退）+ `set_model/set_settings/set_retry/set_thinking_level` setter（模型/设置变更时同步）。
3. **扩展 seam**：AgentSession 所有扩展调用经 `core/extensions.rs` 的 no-op `ExtensionRunner`（has_handlers→false、emit→默认、get_command→None、flag_values/invalidate/on_error 齐备）；`bind_extensions(mode)` 保留签名与事件发射点，T15 替换实现。RPC 的 `extension_ui_request` 帧类型、9 方法名与降级清单以常量/类型形式预留。
4. **ModelRuntime**：组合 rpi-ai `Models` + auth 凭据存储（`{agentDir}/auth.json`）+ `JsonFileModelsStore`（`models.json`）；提供 `get_auth/has_configured_auth/check_auth/is_using_oauth/get_available/refresh/stream_simple/register_provider`。
5. **trust gate**：headless 不提示——`--approve/-a` 信任、`--no-approve/-na` 忽略项目本地、否则 `settings.defaultProjectTrust`（默认 untrusted）；interactive 提示留 T12。
6. **子命令分流**（install/remove/list/update/config）属 T14；T10 仅在 app 入口留分流占位与「未实现」诊断。
7. **首次运行 setup**（主题选择 + analytics opt-in）属 T12；headless 模式不触发。
8. **信号**：SIGTERM→143、SIGHUP→129（print/rpc；杀 detached 子进程经 T06 bash 取消语义）。
9. **测试策略**：单元测试逐模块（args 全标志矩阵、file_processor、model_resolver、rpc 32 命令契约、LF 帧）；对拍走 `rpi-test-support` FauxProvider + `fixtures/generated/{single-turn,tool-calls,steering-followup,abort,length-truncation,compaction*}` 场景驱动 print/json 模式归一化 diff；SDK 外部调用示例测试。

## 完成摘要（2026-08-04）

T10 全部交付并验收通过：CLI 解析（手写解析器，args.test.ts 移植测试 75 个：上游 72 + 3 补充）、
ModelRuntime/ModelResolver、AgentSession 体系、启动管线（`app.rs`）、
print/json 模式（`modes/print_mode.rs`）、rpc 模式（`modes/rpc.rs`，32 命令
逐条契约 + 严格 LF 帧 + 关闭语义 + session 替换 rebind）、`rpi-rpc` bin、
SDK 表面（`sdk.rs`）。测试：`rpc_mode_test.rs` 17、`agent_session_test.rs` 5、
`parity_headless_test.rs` 9（5 场景 fixtures 归一化 diff + print 模式四例）、
`sdk_example_test.rs` 1。偏离 D-015 登记并回写（`02-design.md` §6.1/§6.3/§6.6/§12）。
遗留边界：`export_html` 与 install 等子命令占位（T14）、interactive 模式（T12）、
扩展 UI 真实往返（T15，协议层已预留）。

## 进度跟踪

- [x] 设计细化
- [x] 实现
- [x] 自测
- [x] 门禁验收
- [x] 文档回写

## 自测清单

- [x] §3.1 标志全集解析测试（`cli/args.rs` 移植 `args.test.ts` 75 测试（上游 72 + 3 补充），含组合、互斥、`@file`、扩展标志透传、diagnostics 分级）
- [x] 非 TTY 自动降级 print；interactive + piped stdin 降级（`app.rs::resolve_app_mode` + 冒烟验证）
- [x] print：stdin 合并（`cli/initial_message.rs` 单测）、多条消息依次发送（`parity_headless_test::print_mode_sends_all_messages_in_order`）、最终 text 块输出（`print_mode_text_output`）、error/aborted exit 1（`print_mode_error_and_aborted_exit_1`）、信号退出码（`rpc_mode_test::rpc_mode_sigterm_exits_143` / `rpc_mode_sighup_exits_129`）
- [x] json：header + 事件序列 fixtures 归一化 diff 一致（`parity_headless_test` 5 场景 + `print_mode_json_event_stream`）
- [x] rpc：32 命令逐条契约测试（`rpc_mode_test.rs`，32/32）；严格 LF 帧（U+2028/2029 payload 不错拆、CRLF 容忍、EOF 尾行）；bash/abort_bash 往返；session 替换后 rebind；关闭语义（EOF exit 0）
- [x] resume / fork / `--session-id` / `--no-session` / header cwd 缺失各路径（`app.rs` 冒烟 + `session_replacement_commands` 的 fork/switch/new_session/clone）
- [x] SDK 示例：crate 外部调用 `create_agent_session` 完成一轮 faux 对话（`sdk_example_test.rs`）

## 门禁验收

通用门禁 G1–G7 全过。

任务特有标准：

- [x] 三模式对拍场景全过（场景清单 + diff 结果附验收记录）
- [x] RPC 命令面与 `docs/rpc.md` 逐条对照清单完成（32/32，附验收记录）
- [x] 需求 §2.2–§2.5、§3.1–§3.3 逐条核对有测试锚点（附验收记录）

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| D-015 | headless 模式 Rust 落地差异（手写 CLI 解析器、provider 生态 T13 子集、占位边界、docs 路径、session_env 动态 cell、资源枚举排序、SessionManager::list 提前） | 已回写 |

## 验收记录

- 验收日期：2026-08-04
- 验收人：实现者自证（按 gates.md §1 单人流程）
- G1 构建/静态检查：通过。`cargo build --workspace` ✓；`cargo clippy --workspace --all-targets -- -D warnings` ✓（0 警告）；`cargo fmt --all -- --check` ✓
- G2 测试：通过。`cargo test --workspace` 全绿（42 个测试目标，0 失败；无 live 测试；非 live 测试全走 FauxProvider，无真实网络）。T10 新增：`rpc_mode_test.rs` 17、`agent_session_test.rs` 5、`parity_headless_test.rs` 9、`sdk_example_test.rs` 1，加上一阶段的 `cli/args.rs` 75 移植测试与 ModelRuntime/ModelResolver 单测
- G3 对拍：通过。场景清单与 diff 结果：
  - session.jsonl + events.jsonl 归一化 diff（`parity_headless_test.rs`）：`single-turn` ✓、`tool-calls` ✓、`steering-followup` ✓、`abort` ✓、`length-truncation` ✓；compaction 两场景由 `parity_compaction_test.rs`（T08）覆盖
  - 归一化口径（沿用既有先例）：`message_update`/`tool_execution_update` 整类排除（delta/分块边界非确定）；`usage`/`details` 键剥离；session 头 cwd 占位；`tool_execution_end` 连续块按 (toolCallId, toolName) 排序（并行完成序非确定）
  - RPC 逐条对拍级基准映射（`docs/rpc.md` → 测试锚点，32/32）：

    | rpc.md 命令 | 测试锚点（`crates/rpi/tests/rpc_mode_test.rs`，除注明外） |
    |---|---|
    | prompt | `prompt_lifecycle_messages_state_stats`（接受/事件流）、`steer_follow_up_abort_during_streaming`（streamingBehavior 排队/缺失拒绝） |
    | steer | `steer_follow_up_abort_during_streaming` |
    | follow_up | `steer_follow_up_abort_during_streaming` |
    | abort | `steer_follow_up_abort_during_streaming` |
    | new_session | `session_replacement_commands`（含 parentSession、cancelled 形状、rebind 后 get_state） |
    | get_state | `prompt_lifecycle_messages_state_stats`（全字段形状） |
    | get_messages | `prompt_lifecycle_messages_state_stats`、`bash_commands` |
    | set_model | `model_and_thinking_commands`（ok + `Model not found: faux/nope`） |
    | cycle_model | `model_and_thinking_commands`（双模型）、`cycle_commands_null_data_paths`（data null） |
    | get_available_models | `model_and_thinking_commands` |
    | set_thinking_level | `model_and_thinking_commands`（+ thinking_level_changed 事件） |
    | cycle_thinking_level | `model_and_thinking_commands`、`cycle_commands_null_data_paths`（data null） |
    | get_available_thinking_levels | `model_and_thinking_commands`、`cycle_commands_null_data_paths`（`["off"]`） |
    | set_steering_mode | `queue_mode_and_toggle_commands` |
    | set_follow_up_mode | `queue_mode_and_toggle_commands` |
    | compact | `compact_command`（compaction_start/end 事件 + CompactionResult 形状） |
    | set_auto_compaction | `queue_mode_and_toggle_commands` |
    | set_auto_retry | `queue_mode_and_toggle_commands` |
    | abort_retry | `queue_mode_and_toggle_commands` |
    | bash | `bash_commands`（BashResult 形状、id 关联 bash_execution_update、bashExecution 消息落库） |
    | abort_bash | `bash_commands`（no-op）、`bash_abort_roundtrip`（cancelled:true） |
    | get_session_stats | `prompt_lifecycle_messages_state_stats` |
    | export_html | `export_html_placeholder`（T14 占位错误，D-015） |
    | switch_session | `session_replacement_commands`（cancelled:false + sessionId/sessionFile 切换） |
    | fork | `entries_tree_fork_messages`（text + cancelled，fork 后空分支） |
    | clone | `session_replacement_commands` |
    | get_fork_messages | `entries_tree_fork_messages` |
    | get_entries | `entries_tree_fork_messages`（全量 + since 游标 + `Entry not found: nope`） |
    | get_tree | `entries_tree_fork_messages`（单根树 + leafId） |
    | get_last_assistant_text | `prompt_lifecycle_messages_state_stats`、`entries_tree_fork_messages`（null） |
    | set_session_name | `session_name_and_get_commands`（+ 空名错误） |
    | get_commands | `session_name_and_get_commands`（空目录）、`get_commands_with_prompt_template`（prompt 模板 + sourceInfo 重建） |
    | 协议项 | 帧（LF 严格/U+2028/U+2029/CRLF/EOF 尾行）、parse 错误、未知命令、字段形状错误、extension_ui_response 路由：`protocol_errors_and_framing`；EOF 退出 0：全部用例 `close_and_wait`；SIGTERM=143/SIGHUP=129：`rpc_mode_sigterm_exits_143`/`rpc_mode_sighup_exits_129`；独立入口：`rpi_rpc_bin_end_to_end` |
  - 需求逐条核对（§2.2–§2.5、§3.1–§3.3）：

    | 需求条目 | 测试锚点 |
    |---|---|
    | §2.2 print（依次发送/最后 text 块/error·aborted exit 1/信号/trust 门） | `print_mode_sends_all_messages_in_order`、`print_mode_text_output`、`print_mode_error_and_aborted_exit_1`、信号两例；trust 两阶段见 `app.rs` + 冒烟 |
    | §2.3 json（header 行 + 事件全集单向流） | `print_mode_json_event_stream` + `parity_headless_test` 5 场景事件 diff |
    | §2.4 rpc（32 命令环） | 上表 32/32 |
    | §2.5 SDK | `sdk_example_test.rs`（Quick Start 等价） |
    | §3.1 标志全集 | `cli/args.rs` 75 移植测试（注：`app.rs` 启动管线目前无测试锚点） |
    | §3.2 子命令 | T14 占位诊断（D-015；`app.rs::PLACEHOLDER_SUBCOMMANDS`） |
    | §3.3 环境变量（bash RPI_* 动态注入） | `agent_session_test` + bash 工具测试（session_env 动态 cell，D-015） |
- G4 红线：通过。`external/pi` 无改动（HEAD `2efa728`）；无 JS/TS 执行能力；未读写 `~/.pi`；session 仅 JSONL；token 估算未动；新增非测试代码无 `unwrap`/`expect`（`rpc.rs` 锁中毒走 `unwrap_or_else(into_inner)` 既有模式）；日志/响应无凭据；范围排除项未引入；session 写入无锁
- G5 线格式：通过。RPC 请求/响应/事件、SourceInfo、BashResult、get_state、树/条目均为 camelCase 并与上游 serde 形状逐条核对（契约测试锚定）
- G6 文档同步：通过。溯源注释齐备（rpc.rs/print_mode.rs/app.rs 文件头）；回写 `02-design.md` §6.1/§6.3/§6.6/§12；`fixtures/README.md` 补齐计划口径更新
- G7 偏离闭环：通过。D-015 登记 + 回写（§6.1/§6.3/§6.6/§12），登记表已更新，无行为级偏离
- 结论：通过
