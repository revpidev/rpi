# T07：SessionManager（JSONL 树）

- **状态**：未开始
- **里程碑**：M3
- **依赖**：T01（路径模块）、T05（消息模型）
- **上游对照**：`packages/coding-agent/src/core/session-manager.ts`、`docs/session-format.md`（逐条对拍级基准）
- **需求章节**：§6.1、§6.2、§6.3、§6.4（部分）、§6.6
- **预估**：0.8–1 人月（M3 共 3–3.5，与 T08/T09/T16 合计）

---

## 目标

实现与 Pi 字节兼容的 JSONL session 存储与树导航，保证「能加载并续跑 Pi 生成的
session（v1–v3 自动迁移）」这一成功标准（需求 §1.2.2）。

## 范围

### In

- 统一路径模块（若 T01 未含，本任务落地）：`~/.pir/agent/sessions/`、目录编码 `--<cwd>--`（**去前导斜杠后 `/`、`\`、`:` → `-`**）、文件名 `<timestamp（:.→-）>_<uuid>.jsonl`、覆盖链 `--session-dir` / `PIR_CODING_AGENT_SESSION_DIR` / `settings.sessionDir`（编码规范 §10.1）
- session 树：`id`（8 hex，randomUUID 前 8 位、碰撞重试 100 次退回完整 UUID）、`parentId`、leaf 分支导航；`getTree` 子节点按 timestamp 排序、孤儿当根
- Header `{type:"session", version, id(uuidv7), timestamp(ISO), cwd, parentSession?}`
- 条目类型全集（header + 9 种，需求 §6.2）：`message` / `model_change` / `thinking_level_change` / `compaction` / `branch_summary` / `custom` / `custom_message` / `label` / `session_info`（serde camelCase，编码规范 §4.4）；`custom` 不进 LLM context、`custom_message` 进
- `compaction` 条目两种形态：`firstKeptEntryId`（主路径，只写这种）/ 内嵌 `retainedTail`（**读取兼容**，harness 产物，ADR-0003 §1）；harness 独有 `active_tools_change` / `leaf` 识别并原样保留
- 加载迁移 v1 → v2（加 id/parentId；`firstKeptEntryIndex` 数字下标 → `firstKeptEntryId`）→ v3（`hookMessage` → `custom`）；迁移后**整文件重写**
- **延迟落盘**：首个 assistant 消息前不创建文件（`flushed` 标志 + `wx` 独占创建）
- **无文件锁**（append 直写；与上游一致——锁仅 auth/settings/trust，G4 红线）
- 读取健壮性：1MB 缓冲流式读、跳过畸形行；header 专用 4KB 缓冲 / 1MB 扫描上限（超限回退全量加载）
- `build_context_entries()` / `build_session_context()`：路径上**最后一个** compaction 生效；输出 = compaction + `firstKeptEntryId` 起 + 其后条目；model 取最后 assistant 或 `model_change`；thinkingLevel 默认 `"off"`
- `createBranchedSession`（抽单路径，**label 剔除并按新 parentId 重链**）、`forkFrom`（新 header + parentSession + 全量原样拷贝 + `wx`；`position: before|at` 与 user-message 校验）
- `appendLabelChange`（空值清除）、`appendSessionInfo`（`\r\n` → 空格 sanitize）
- `--no-session` 内存会话
- 降级策略（需求 §6.6）：未知 / `custom` entry 原样保留、不进 LLM context、写回不丢数据
- import / export JSONL 底层能力（CLI 接线在 T10/T14）

### Out

- compaction 生成逻辑（T08，本任务只含 compaction 条目的读写与 context 重建规则）
- `/tree` `/fork` `/clone` 交互（T12）；`--fork` CLI 在 T10
- harness 的 SessionStorage 实现（T16；trait 设计本任务需与之同构对齐）

## 开发要点

- 线格式 serde 形状逐字段核对 `session-format.md`，用 fixtures 对拍兜底
- 追加写时机与上游一致（message_end / tool / model_change 等事件点）；写失败上抛 `Result`
- retainedTail / `firstKeptEntryId` 两种形态都要能用 Pi fixtures 验证
- **不实现** Pi migrations.ts 的 legacy 启动迁移（ADR-0003 §3，G4 红线）

## 进度跟踪

- [ ] 设计细化
- [ ] 实现
- [ ] 自测
- [ ] 门禁验收
- [ ] 文档回写

## 自测清单

- [ ] v1/v2/v3 fixtures 加载并自动迁移，迁移后结构与上游一致（含 firstKeptEntryIndex 转换）
- [ ] `build_context_entries` 对 compaction / branch_summary fixtures 的裁剪结果与上游一致（两种形态）
- [ ] 未知 entry 类型：加载保留 → 写回无损（往返测试）
- [ ] 延迟落盘：首个 assistant 前文件不存在，之后 `wx` 创建
- [ ] label 重链、forkFrom 拷贝语义、session_info sanitize
- [ ] 读取健壮性：畸形行跳过、超大 header 回退
- [ ] 路径覆盖链优先级测试（flag > env > settings > 默认）；目录编码规则（含 Windows 盘符冒号）
- [ ] 追加写失败（只读目录模拟）上抛错误，不 panic

## 门禁验收

通用门禁 G1–G7 全过（G4 重点：仅 JSONL、不读 `~/.pi`、session 无锁；G5 重点：JSONL 形状）。

任务特有标准：

- [ ] Pi 生成的 session fixtures 加载 + 续跑（faux）对拍一致
- [ ] 需求 §6.6 三条降级策略各有测试锚点
- [ ] `session-format.md` 逐条对拍映射表（G3）

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| — | （暂无） | — |

## 验收记录

（待填写，模板见 `gates.md` §3）
