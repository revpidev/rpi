# T10：Headless 模式 — print / json / rpc

- **状态**：未开始
- **里程碑**：M4
- **依赖**：T03、T04、T05、T06、T07、T08、T09
- **上游对照**：`packages/coding-agent/src/cli/*`、`src/modes/*`、`docs/rpc.md`、`docs/json.md`、`docs/sdk.md`
- **需求章节**：§2.2–§2.5、§3.1
- **预估**：1.5–2 人月

---

## 目标

打通启动管线与三种 headless 运行模式，交付可脚本化使用的 `pir`：
print 打印最终文本、json 输出事件流、rpc 提供命令环，同时沉淀 Rust SDK 表面。

## 范围

### In

- CLI 解析（clap）：需求 §3.1 主命令标志全集（`--provider/--model/--api-key`、`-p`、`-c/-r`、`--session*`、`--fork`、工具开关、`--thinking`、`-e/-ne`、`--skill`、`--prompt-template`、`--theme`、`--approve/--no-approve`、`--export`、`--list-models`、`--verbose/--offline` 等）
- 启动管线（设计文档 §6.1）：解析参数 → agent_dir/cwd/offline → SettingsManager → trust gate（非交互不提示）→ services（ResourceLoader / ModelRuntime / Tools）→ SessionManager → AgentSession → mode 分发
- `AgentSession` / `AgentSessionRuntime`：prompt / steer / follow_up / abort、compaction 接入、事件映射 `AgentSessionEvent`、JSONL 持久化接线
- print 模式：初始 prompt（含 piped stdin 合并）→ 打印最终助手文本 → 退出
- json 模式：session header + `AgentSessionEvent` JSONL 单向流
- rpc 模式：**严格 LF** JSONL 帧、命令分发器、`type:"response"` + 异步 events、命令面对齐 `docs/rpc.md`
- Rust SDK 表面：`create_agent_session` / `create_agent_session_runtime` / `SessionManager` / `ModelRuntime` 公开 API

### Out

- RPC 扩展 UI 往返（T15 扩展宿主就绪后补齐；本任务协议层预留）
- interactive 模式（T12）
- `--export` HTML（T14；本任务留参数占位）

## 开发要点

- RPC 与后续 Interactive 共享 `AgentSessionRuntime`，避免两套会话逻辑（设计文档 §6.6）
- RPC 帧按 LF 拆分，不得按 Unicode 行分隔符（需求 §2.4）
- `/new`、切 cwd、resume 时重建 cwd 绑定服务（设计文档 §6.1）
- 三模式的对拍场景走 T02 fixtures：单轮问答、read/bash 工具、steering/follow-up、compaction

## 进度跟踪

- [ ] 设计细化
- [ ] 实现
- [ ] 自测
- [ ] 门禁验收
- [ ] 文档回写

## 自测清单

- [ ] §3.1 标志全集解析测试（含组合与冲突语义）
- [ ] print：stdin 合并、最终文本输出、退出码语义
- [ ] json：header + 事件序列 fixtures 归一化 diff 一致
- [ ] rpc：逐命令契约测试；严格 LF 帧（含 Unicode 行分隔符的 payload 不错拆）
- [ ] resume / fork / `--session-id` / `--no-session` 各路径
- [ ] SDK 示例：crate 外部调用 `create_agent_session` 完成一轮 faux 对话

## 门禁验收

通用门禁 G1–G7 全过。

任务特有标准：

- [ ] 三模式对拍场景全过（场景清单 + diff 结果附验收记录）
- [ ] RPC 命令面与 `docs/rpc.md` 对照清单完成
- [ ] 需求 §2.2–§2.5 逐条核对有测试锚点

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| — | （暂无） | — |

## 验收记录

（待填写，模板见 `gates.md` §3）
