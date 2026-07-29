# T08：Compaction

- **状态**：未开始
- **里程碑**：M3
- **依赖**：T07
- **上游对照**：`packages/coding-agent/src/core/compaction/{compaction,branch-summarization,utils}.ts`、`docs/compaction.md`（逐条对拍级基准）、`packages/ai/src/utils/overflow.ts`
- **需求章节**：§6.5
- **预估**：0.7–1 人月（M3 共 3–3.5，与 T07/T09/T16 合计）

---

## 目标

字节级对齐 Pi 的 compaction：token 估算、切点搜索、summary 生成与条目写入，
用黄金用例对拍锁死（ADR-0002 §4 不允许任何偏差）。

## 范围

### In

- `estimate_tokens` 逐字节移植：`ceil(chars/4)`；image=4800 chars；toolCall=`name.length + JSON.stringify(args).length`；bashExecution=command+output；summary 类=summary.length（**禁止** tiktoken 或任何「改进」）
- `calculate_context_tokens`（totalTokens || 分量合成）与 `estimate_context_tokens`（最后**有效** usage 锚点 + trailing；跳过 aborted/error/全零）
- **触发双路**：agent_end 后 + **每次 prompt 提交前**（捕获 aborted 超窗）；`contextTokens > contextWindow - reserveTokens`
- **overflow 恢复**：三分支判定（pattern 表 / z.ai silent / Xiaomi 截断式）；仅同模型；只尝试一次；失败发 compaction_end 错误；stop 的 overflow 只压缩不重试；stale usage 时间戳守卫
- 参数：`compaction.enabled` / `reserveTokens`(16384) / `keepRecentTokens`(20000)；`branchSummary.{reserveTokens:16384,skipPrompt:false}`（settings 接线在 T09）
- **切点 `findCutPoint`**：倒序累积 ≥ keepRecentTokens；合法切点类型白名单（**绝不切 toolResult**）；前向吸收元数据条目（遇 compaction 边界停）；split turn（turnStartIndex）
- **三个 summary prompt**（逐字移植）：初始（Goal/Constraints/Progress/Key Decisions/Next Steps/Critical Context）、迭代更新（`<previous-summary>`）、turn 前缀；`Additional focus:` 拼接 customInstructions；**另须字节级对齐**：共用 system prompt `SUMMARIZATION_SYSTEM_PROMPT`（utils.ts:156）、split-turn 合并格式串 `\n\n---\n\n**Turn Context (split turn):**\n\n`（compaction.ts:881）、占位串 "No prior history."（compaction.ts:846）
- 预算：history maxTokens=min(0.8×reserveTokens, model.maxTokens)；turn prefix=0.5×reserveTokens
- 序列化：`<conversation>` 包裹、`[User]:` 等格式、tool result 截 2000 chars；文件操作跟踪 → `<read-files>`/`<modified-files>` + `details.{readFiles,modifiedFiles}` 跨 compaction 累积
- 请求隔离：`cacheRetention:"none"` + 新 uuidv7 routing session id + 复用 `settings.retry`（`summarization_retry_*` 三类事件）；reasoning 模型带 thinkingLevel
- 重复压缩从上次 kept boundary 起算并重算 `tokensBefore`
- auto-compaction 后队列非空则 `agent.continue()` 一次
- **branch summarization**：公共祖先；倒序装填 `contextWindow − reserveTokens`（summary 类 90% 预算强留）；maxTokens 2048；preamble；label 挂 summary 条目（`/tree` 交互在 T12）
- 手动 compact 的 API 层能力（`/compact [instructions]` 接线在 T10/T12）
- 共享常量模块落地在 `pir-agent`（供 T16 harness 复用，设计文档 §4.5）

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

- [ ] token 估算黄金用例集：多组 fixture 数值与上游完全一致（含 image/toolCall/bashExecution 各形态）
- [ ] 切点搜索：构造的边界 session（恰好跨阈值、split turn 落点、toolResult 边界）结果与上游一致
- [ ] 迭代 summary：多轮压缩后 `firstKeptEntryId` / `tokensBefore` 正确
- [ ] overflow 三分支各触发路径 + 同模型守卫 + 一次恢复限制
- [ ] compaction 触发场景（faux，双路触发）事件序列与 fixtures 归一化 diff 一致
- [ ] summary prompt 模板渲染结果与上游逐字节比对
- [ ] 文件操作跟踪与 `<read-files>`/`<modified-files>` 累积正确
- [ ] branch summary：预算装填、90% 强留、maxTokens 2048

## 门禁验收

通用门禁 G1–G7 全过（G4 重点：token 估算算法未偏离）。

任务特有标准：

- [ ] token 估算与切点的黄金对拍全部通过（验收记录附数值表）
- [ ] 需求 §6.5 各条目（触发/参数/切点/prompt/预算/重算/routing session/cache write/branch）逐条核对有测试锚点
- [ ] `compaction.md` 逐条对拍映射表（G3）

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| — | （暂无） | — |

## 验收记录

（待填写，模板见 `gates.md` §3）
