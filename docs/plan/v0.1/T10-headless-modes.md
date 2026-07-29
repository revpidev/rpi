# T10：Headless 模式 — print / json / rpc

- **状态**：未开始
- **里程碑**：M4
- **依赖**：T03、T04、T05、T06、T07、T08、T09
- **上游对照**：`packages/coding-agent/src/cli/*`、`src/main.ts`、`src/modes/*`、`src/rpc-entry.ts`、`docs/rpc.md`（逐条对拍级基准）、`docs/json.md`、`docs/sdk.md`、`docs/usage.md`
- **需求章节**：§2.2–§2.5、§3.1–§3.3
- **预估**：1.5–2 人月

---

## 目标

打通启动管线与三种 headless 运行模式，交付可脚本化使用的 `pir`：
print 打印最终文本、json 输出事件流、rpc 提供 32 命令环，同时沉淀 Rust SDK 表面。

## 范围

### In

- CLI 解析（clap）：需求 §3.1 主命令标志全集**含精确语义**：
  - `@file` 位置参数（文本 `<file name>` 标签 / 图片 ImageContent resize 2000×2000 / 空文件跳过 / 不存在 exit 1；RPC 模式禁止）
  - `-p` 值吞噬规则；`--model provider/pattern:thinking` 解析；`--models` glob/模糊/`:thinking`；`--api-key` 必须搭配 model（非持久 override）
  - `--session` 三级解析（路径 → 本项目 id → 全局跨项目 + 交互确认 fork）；`--session-id` 正则校验与「不存在则新建」；`--fork`/`--session-id` 互斥矩阵
  - **未知 `--flag` 收集为扩展标志**（`extensionFlagValues` 透传，help 动态段）；单 `-x` 未知为 error diagnostic
  - diagnostics 体系（warning/error，error exit 1；非法 thinking level 仅 warning）
- 启动管线（设计文档 §6.1）：`--offline` → 同时设 `PIR_SKIP_VERSION_CHECK`；模式解析 rpc > json > print（-p 或**非 TTY 自动**，含 interactive + piped stdin 降级）→ SettingsManager → trust gate（非交互不提示）→ services → SessionManager（含 header cwd 缺失处理）→ AgentSession → mode 分发；**不实现** migrations.ts（ADR-0003 §3）
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
  - 独立入口 `pir-rpc` bin（等价 `--mode rpc`）
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

## 进度跟踪

- [ ] 设计细化
- [ ] 实现
- [ ] 自测
- [ ] 门禁验收
- [ ] 文档回写

## 自测清单

- [ ] §3.1 标志全集解析测试（含组合、互斥、`@file`、扩展标志透传、diagnostics 分级）
- [ ] 非 TTY 自动降级 print；interactive + piped stdin 降级
- [ ] print：stdin 合并、多条消息依次发送、最终 text 块输出、error/aborted exit 1、信号退出码
- [ ] json：header + 事件序列 fixtures 归一化 diff 一致（事件全集）
- [ ] rpc：32 命令逐条契约测试；严格 LF 帧（含 U+2028/2029 payload 不错拆）；bash/abort_bash 往返；session 替换后 rebind；关闭语义
- [ ] resume / fork / `--session-id` / `--no-session` / header cwd 缺失各路径
- [ ] SDK 示例：crate 外部调用 `create_agent_session` 完成一轮 faux 对话

## 门禁验收

通用门禁 G1–G7 全过。

任务特有标准：

- [ ] 三模式对拍场景全过（场景清单 + diff 结果附验收记录）
- [ ] RPC 命令面与 `docs/rpc.md` 逐条对照清单完成（32/32）
- [ ] 需求 §2.2–§2.5、§3.1–§3.3 逐条核对有测试锚点

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| — | （暂无） | — |

## 验收记录

（待填写，模板见 `gates.md` §3）
