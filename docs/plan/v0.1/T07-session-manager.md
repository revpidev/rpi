# T07：SessionManager（JSONL 树）

- **状态**：未开始
- **里程碑**：M3
- **依赖**：T01（路径模块）、T05（消息模型）
- **上游对照**：`packages/coding-agent/src/core/session-manager.ts`、`docs/session-format.md`
- **需求章节**：§6.1、§6.2、§6.3（部分）、§6.5
- **预估**：0.8–1 人月（M3 共 2–3，与 T08/T09 合计）

---

## 目标

实现与 Pi 字节兼容的 JSONL session 存储与树导航，保证「能加载并续跑 Pi 生成的
session（v1–v3 自动迁移）」这一成功标准（需求 §1.2.2）。

## 范围

### In

- 统一路径模块（若 T01 未含，本任务落地）：`~/.pir/agent/sessions/`、`<cwd>` 编码目录、覆盖链 `--session-dir` / `PIR_CODING_AGENT_SESSION_DIR` / `settings.sessionDir`（编码规范 §10.1）
- session 树：`id`（8 hex）、`parentId`、leaf 分支导航
- JSONL 追加写：message_end / tool / model_change 等事件点触发（与上游一致）；写失败上抛 `Result`
- 条目类型全集：`message` / `model_change` / `thinking_level_change` / `compaction` / `branch_summary` / `custom` / `label`（serde camelCase，编码规范 §4.4）
- 加载迁移 v1 → v2 → v3；`compaction` 条目两种形态（`firstKeptEntryId` 旧形态 / 内嵌 `retainedTail` 新形态）
- `build_context_messages()`：compaction / branch_summary 的 retainedTail 规则（对拍重点，禁止凭理解重写）
- 文件锁 `fs2`；`--no-session` 内存会话
- 降级策略（需求 §6.5）：未知 / `custom` entry 原样保留、不进 LLM context、写回不丢数据
- import / export JSONL 底层能力（CLI 接线在 T10/T14）

### Out

- compaction 生成逻辑（T08，本任务只含 compaction 条目的读写与 context 重建规则）
- `/tree` `/fork` `/clone` 交互（T12）；`--fork` CLI 在 T10

## 开发要点

- 线格式 serde 形状逐字段核对 `session-format.md`，用 fixtures 对拍兜底
- 追加写与锁的获取/释放在 `SessionManager` 内闭环
- retainedTail / `firstKeptEntryId` 两种形态都要能用 Pi fixtures 验证

## 进度跟踪

- [ ] 设计细化
- [ ] 实现
- [ ] 自测
- [ ] 门禁验收
- [ ] 文档回写

## 自测清单

- [ ] v1/v2/v3 fixtures 加载并自动迁移，迁移后结构与上游一致
- [ ] `build_context_messages` 对 compaction / branch_summary fixtures 的 retainedTail 裁剪结果与上游一致
- [ ] 未知 entry 类型：加载保留 → 写回无损（往返测试）
- [ ] 文件锁：并发打开同一会话的互斥语义正确
- [ ] 路径覆盖链优先级测试（flag > env > settings > 默认）
- [ ] 追加写失败（只读目录模拟）上抛错误，不 panic

## 门禁验收

通用门禁 G1–G7 全过（G4 重点：仅 JSONL、不读 `~/.pi`；G5 重点：JSONL 形状）。

任务特有标准：

- [ ] Pi 生成的 session fixtures 加载 + 续跑（faux）对拍一致
- [ ] 需求 §6.5 三条降级策略各有测试锚点

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| — | （暂无） | — |

## 验收记录

（待填写，模板见 `gates.md` §3）
