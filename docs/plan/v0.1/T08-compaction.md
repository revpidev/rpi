# T08：Compaction

- **状态**：未开始
- **里程碑**：M3
- **依赖**：T07
- **上游对照**：`packages/coding-agent/src/core/compaction/{compaction,branch-summarization,utils}.ts`、`docs/compaction.md`
- **需求章节**：§6.4
- **预估**：0.7–1 人月（M3 共 2–3，与 T07/T09 合计）

---

## 目标

字节级对齐 Pi 的 compaction：token 估算、切点搜索、summary 生成与条目写入，
用黄金用例对拍锁死（ADR-0002 §4 不允许任何偏差）。

## 范围

### In

- `estimate_tokens` 逐字节移植（chars/4 启发式；**禁止** tiktoken 或任何「改进」）
- 触发逻辑：`contextTokens > contextWindow - reserveTokens`；overflow 恢复重试
- 参数：`compaction.enabled` / `reserveTokens`(16384) / `keepRecentTokens`(20000)（settings 接线在 T09）
- 切点搜索、split turn、迭代 summary、`firstKeptEntryId`、`tokensBefore` 重算
- summary prompt 模板字节级对齐（便于对拍）
- 压缩请求使用独立 routing session id；支持处关闭 prompt-cache write
- branch summarization（`/tree` 导航的底层能力；交互在 T12）
- 手动 compact 的 API 层能力（`/compact [instructions]` 接线在 T10/T12）

### Out

- 扩展自定义 compaction（`session_before_compact` 等钩子，T15）
- 交互式 `/compact` `/tree` UI（T12）

## 开发要点

- 本模块是「禁止凭理解重写」的头号区域：每处语义标注上游行级溯源注释
- 纯函数集中一个模块，配黄金用例（固定输入 → 固定估算值/切点）
- summary 生成走 `StreamFn` 注入路径，测试用 faux 驱动

## 进度跟踪

- [ ] 设计细化
- [ ] 实现
- [ ] 自测
- [ ] 门禁验收
- [ ] 文档回写

## 自测清单

- [ ] token 估算黄金用例集：多组 fixture 数值与上游完全一致
- [ ] 切点搜索：构造的边界 session（恰好跨阈值、split turn 落点）结果与上游一致
- [ ] 迭代 summary：多轮压缩后 `firstKeptEntryId` / `tokensBefore` 正确
- [ ] compaction 触发场景（faux）事件序列与 fixtures 归一化 diff 一致
- [ ] summary prompt 模板渲染结果与上游逐字节比对

## 门禁验收

通用门禁 G1–G7 全过（G4 重点：token 估算算法未偏离）。

任务特有标准：

- [ ] token 估算与切点的黄金对拍全部通过（验收记录附数值表）
- [ ] 需求 §6.4 各条目（触发/参数/重算/routing session/cache write）逐条核对有测试锚点

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| — | （暂无） | — |

## 验收记录

（待填写，模板见 `gates.md` §3）
