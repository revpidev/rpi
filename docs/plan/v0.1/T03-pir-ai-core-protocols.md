# T03：pir-ai 核心协议（Anthropic + OpenAI 系）

- **状态**：未开始
- **里程碑**：M1
- **依赖**：T01（契约测试基建依赖 T02 的部分可并行接入）
- **上游对照**：`packages/ai/src/api/{anthropic-messages,openai-completions,openai-responses,openai-responses-shared}.ts`、`packages/ai/src/types.ts`、`packages/ai/src/utils/*`、`packages/ai/src/models.ts`
- **需求章节**：§5.1、§5.2（三适配器锚点）、§5.5（横切能力的基础设施部分）
- **预估**：2.5–3 人月（M1 共 3–4，与 T04 合计）

---

## 目标

实现 `pir-ai` 的类型层完备化、首批 3 个 API 适配器与横切基础设施，
打通「Context → 协议流式事件」的核心链路，供 M2 的 agent loop 与后续 headless 模式使用。

## 范围

### In

- `ApiStream` trait 与 `Models::stream` / `stream_simple` 分发（设计文档 §3.3；含混合 API provider 的 api map 分发与缺 API 报错）
- 适配器（文件名与上游一一对应）：
  - `api/anthropic_messages.rs`（自适应/预算 thinking 双轨、cache_control、`x-session-affinity`、usage 从 message_start 捕获、refusal/pause_turn 映射、tool call id 归一化；**OAuth 伪装与工具名映射随 T04**）
  - `api/openai_completions.rs`（**compat URL 自动检测矩阵基础设施** `detect_compat` + `get_compat` 部分覆盖回落；compat 为 21 字段、`thinkingFormat` 10 取值（完整名单见需求 §5.2）；`prompt_cache_key`/`store:false`/`stream_options.include_usage`；usage 兜底 `choice.usage`；session affinity 三格式）
  - `api/openai_responses.rs` + `openai_responses_shared.rs`（encrypted reasoning 持久化回填、`TextSignatureV1`、max_output_tokens 下限 16；tool call id 为 `call_id|item_id` 复合格式、跨模型 `fc_<hash>` 重建，openai-responses-shared.ts:151-168,472,492）
- **stream 不抛出契约**：stream 调用后一切失败（auth/abort/校验/网络）编码为 `StreamEvent::Error` + `stopReason:"error"/"aborted"`，不返回 Err
- 流式增量解析：text / thinking 增量透传（**事件按 `content_index` 关联，允许交错**）；partial tool-call JSON 流式累积 + 结束校验（`json_parse.rs`：partial JSON + repair）
- thinking 级别统一映射（`stream_simple` 层：`reasoning` 无 `off`，off=省略；`thinkingBudgets` 仅 minimal/low/medium/high；clamp 先上后下；xhigh/max 预算路径降为 high；默认预算 1024/2048/8192/16384、minOutput 1024）
- `clamp_max_tokens_to_context`（contextWindow − 估算 − 4096 安全余量，下限 1）
- usage / cost / cache 统计：`calculateCost`（cost.tiers 阶梯、cacheWrite1h 2× 费率）；totalTokens 分量合成
- 工具参数 JSON Schema 校验 + **宽松类型强转**（null→0、"123"→123）；`sanitize_surrogates`
- 横切基础设施（`pir-ai/src/utils/`，设计文档 §3.6）：
  - `transform_messages.rs`（孤儿 tool call 合成 "No result provided"、error/aborted 不回放、非 vision 图片占位符、跨模型 thinking 转文本、id 归一化回填）
  - `provider_retry.rs` + `retry.rs`（两层重试：`x-should-retry`/retry-after/60s 上限立即失败；外层错误分类 regex 表）
  - `overflow.rs`（三分支：pattern 表 / silent / 截断式）
  - `estimate.rs`（chars/4、image=4800、usage 锚点 trailing）
  - `error_body.rs`、`headers.rs`（大小写不敏感合并、null 删除）、`deferred_tools.rs`
- `StreamOptions` 全量（含 `session_id`、`cache_retention`、`on_payload`/`on_response`/`transform_headers`、`env`、`max_retry_delay_ms`）
- `models.json` 加载与内置模型注册机制；生成数据管线（`build.rs` 占位，正式刷新在 T13/T14）
- image input（base64 data block）支持
- `ModelsStore` 持久化骨架（models/lastModified/checkedAt/etag；远程 overlay 在 T13）

### Out

- 其余 7 种 KnownApi 与全量 provider、compat 矩阵全量数据（T13）
- OAuth 流程与凭据存储（T04；本任务只定义 auth 解析接口）
- image generation 子系统（T13）

## 开发要点

- 移植以上游 vitest 行为为金标准，逐适配器核对事件序；禁止「看起来像」式实现（可行性 §3.1 风险）
- compat 检测矩阵用表驱动表达（设计文档 §13 开放项），T03 先落 anthropic/openai/deepseek 等本任务所需子集，结构须支持 T13 全量扩展
- 每个适配器文件头部标注溯源注释（编码规范 §14.3）
- HTTP 用 `reqwest`（rustls）；SSE 解析与上游帧边界语义对齐（含 `parseJsonWithRepair`）
- live 测试用 `PIR_LIVE_TEST=1` 门禁，默认跳过（编码规范 §12.6）

## 进度跟踪

- [ ] 设计细化
- [ ] 实现
- [ ] 自测
- [ ] 门禁验收
- [ ] 文档回写

## 自测清单

- [ ] 上游 `packages/ai` 相关 vitest 用例意图移植为同名 Rust 测试并通过
- [ ] 三适配器契约测试：固定请求 → 期望事件序列（录制/构造的 SSE 流驱动）
- [ ] content_index 交错事件：消费者按 index 正确关联（非连续 block 顺序）
- [ ] stream 不抛出：HTTP 错误 / 流中断 / abort / 校验失败 → 全部 Error 事件 + stopReason，无 panic、无 Err
- [ ] partial tool-call JSON：分片任意切分下累积结果一致；非法 JSON 结束时按上游语义报错
- [ ] compat 矩阵：检测值命中、部分覆盖回落检测值、显式覆盖优先
- [ ] thinking 映射：各级别对三协议的选项映射正确（含 off=省略、xhigh 降 high）
- [ ] maxTokens 钳制与 thinking 预算默认表数值正确
- [ ] transform_messages：孤儿 tool call / error 不回放 / 图片占位符各用例
- [ ] 两层重试：60s 上限立即失败、retry-after 解析、错误分类表命中
- [ ] usage/cost 累计与上游字段语义一致（含 tiers、cacheWrite1h）

## 门禁验收

通用门禁 G1–G7 全过。

任务特有标准：

- [ ] 3 个适配器契约测试全过，事件类型序列与上游测试语义一致
- [ ] live smoke（有 key 时）：Anthropic 与 OpenAI 各完成一次真实流式调用（结果记录；无 key 则记录豁免理由）
- [ ] `Models::stream` 按 `model.api` 正确分发，未注册 api 报明确错误
- [ ] 需求 §5.5 横切条目（本任务范围内）逐条核对有测试锚点

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| — | （暂无） | — |

## 验收记录

（待填写，模板见 `gates.md` §3）
