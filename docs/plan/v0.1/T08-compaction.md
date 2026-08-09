# T08：Compaction

- **状态**：已完成
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
- 共享常量模块落地在 `rpi-agent`（供 T16 harness 复用，设计文档 §4.5）

### Out

- 扩展自定义 compaction（`session_before_compact` 等钩子，T15）
- 交互式 `/compact` `/tree` UI（T12）

## 开发要点

- 本模块是「禁止凭理解重写」的头号区域：每处语义标注上游行级溯源注释
- 纯函数集中一个模块，配黄金用例（固定输入 → 固定估算值/切点）
- summary 生成走 `StreamFn` 注入路径，测试用 faux 驱动

## 进度跟踪

- [x] 设计细化
- [x] 实现
- [x] 自测
- [x] 门禁验收
- [x] 文档回写

## 自测清单

- [x] token 估算黄金用例集：多组 fixture 数值与上游完全一致（含 image/toolCall/bashExecution 各形态）——`rpi-agent/tests/compaction_golden_test.rs`（estimateTokens 电池）
- [x] 切点搜索：构造的边界 session（恰好跨阈值、split turn 落点、toolResult 边界）结果与上游一致——`compaction_golden_test.rs`（findCutPoint 电池）
- [x] 迭代 summary：多轮压缩后 `firstKeptEntryId` / `tokensBefore` 正确——`compaction-threshold` fixture 对拍（两轮压缩 tokensBefore 5806/3305）
- [x] overflow 三分支各触发路径 + 同模型守卫 + 一次恢复限制——`rpi/tests/compaction_runner_test.rs`（10 用例）+ `compaction-overflow` fixture 对拍
- [x] compaction 触发场景（faux，双路触发）事件序列与 fixtures 归一化 diff 一致——`rpi/tests/parity_compaction_test.rs`（2 场景）
- [x] summary prompt 模板渲染结果与上游逐字节比对——`compaction_golden_test.rs`（`compaction/prompts/*.txt` 全部比对）
- [x] 文件操作跟踪与 `<read-files>`/`<modified-files>` 累积正确——`compaction_golden_test.rs`（extractFileOperations / compact 电池）
- [x] branch summary：预算装填、90% 强留、maxTokens 2048——`compaction_golden_test.rs`（prepareBranchEntries / branch 电池）

## 门禁验收

通用门禁 G1–G7 全过（G4 重点：token 估算算法未偏离）。

任务特有标准：

- [x] token 估算与切点的黄金对拍全部通过（验收记录附数值表）
- [x] 需求 §6.5 各条目（触发/参数/切点/prompt/预算/重算/routing session/cache write/branch）逐条核对有测试锚点
- [x] `compaction.md` 逐条对拍映射表（G3）

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| D-013 | compaction 移植 Rust 落地差异（算法层落 `rpi-agent::compaction` + 触发接线 `rpi::core::compaction_runner`、`StreamOptions.reasoning` 字段、session 共享函数下沉 3 项） | 已回写 |

## 验收记录

- 验收日期：2026-08-03
- 验收人：kimi-code（单人开发，按清单逐项自证，命令实跑）
- G1 构建/静态检查：通过（`cargo build --workspace` / `cargo clippy --workspace --all-targets -- -D warnings` / `cargo fmt --all -- --check` 全部零警告）
- G2 测试：通过（`cargo test --workspace` 30 个套件 840 passed, 0 failed；无 live 测试）
- G3 对拍：通过
  - `rpi/tests/parity_compaction_test.rs` 2 场景（`compaction-threshold` / `compaction-overflow`）与 fixtures 归一化 diff 一致；归一化剥离 `usage`/`tokensBefore`/`estimatedTokensAfter` 键值（faux usage 双计，无法复现上游数值），触发决策、事件序、条目结构全比对
  - `compaction.md` 逐条对拍映射表见 `fixtures/README.md` §5.3（触发/切点/split turn、CompactionEntry、Summary 模板、消息序列化、Settings 均 ✅ T08；扩展钩子 ⏳ T15；BranchSummaryEntry 持久化 ⏳ T12/T16）
  - 数值锚点：`compaction-threshold` 两轮压缩 tokensBefore 5806/11292、overflow 3305；estimatedTokensAfter 3111/2980/474；overflow `willRetry:true`（stop 分支 `false`）——均与上游 fixture 一致
- G4 红线：通过（`external/pi` `git status --porcelain` 为空且 HEAD=`2efa728`；未引入 JS 执行能力/SQLite/rg|fd 下载；未读写 `~/.pi`/`.pi`；session 写入无文件锁；新增非测试代码无 `unwrap()`/`expect()`；无凭据入日志；token 估算算法与常量由黄金用例锁死未偏离；无新增依赖）
- G5 线格式：通过（CompactionEntry/BranchSummaryEntry camelCase serde 形状随 fixtures 对拍覆盖，见 §5.3 映射表）
- G6 文档同步：通过（移植代码行级溯源注释；D-013 已回写 `docs/02-design.md` §4.4/§6.4/§12；`fixtures/README.md` §3/§5.1/§5.3 已更新）
- G7 偏离闭环：通过（D-013 一条，实现细节级，状态「已回写」，无需 ADR）
- 结论：**通过**

任务特有标准：

- token 估算与切点黄金对拍：`rpi-agent/tests/compaction_golden_test.rs` 16/16 通过；`fixtures/generated/compaction/golden.json` 由 `fixtures/generate-compaction-golden.mjs` 驱动上游 dist 真函数产出，覆盖 estimateTokens（user text/block+image/toolCall/bashExecution/summary 各形态）、calculateContextTokens、estimateContextTokens、findCutPoint（恰好跨阈值/split turn/toolResult 边界）、prepareCompaction、serializeConversation、文件操作、prepareBranchEntries、isContextOverflow；全部 summarization prompt（history 初始/更新、turn prefix、split-turn 合并、文件列表追加、branch、preamble、system prompt）与上游逐字节比对（`compaction/prompts/*.txt`）
- 需求 §6.5 逐条锚点：token 估算/上下文 token/切点/三个 prompt/预算/序列化/文件跟踪 → golden 测试电池；触发双路/aborted 跳过/同模型守卫/stale 守卫/一次恢复/失败 compaction_end 错误/禁用短路/手动 compact 成功与错误路径 → `rpi/tests/compaction_runner_test.rs` 10 用例；双路触发事件序/重算 tokensBefore/overflow 恢复（willRetry）→ parity 2 场景；cacheRetention:"none"/uuidv7 routing session id/maxTokens 预算 → golden 选项断言；`summarization_retry_*` 复用 settings.retry → runner 接线（settings 文件接线属 T09）；branch summary（公共祖先/90% 强留/maxTokens 2048/preamble/label）→ golden branch 电池
