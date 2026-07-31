# T03：pir-ai 核心协议（Anthropic + OpenAI 系）

- **状态**：已完成
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
  - `api/anthropic_messages.rs`（自适应/预算 thinking 双轨、cache_control、`x-session-affinity`、usage 从 message_start 捕获、refusal/pause_turn 映射、tool call id 归一化、OAuth 伪装身份头与工具名映射 `to/from_claude_code_name`/`CLAUDE_CODE_TOOLS`——已随本任务落地，OAuth 登录流程仍随 T04）
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
- OAuth 登录/刷新流程与凭据存储（T04；本任务只定义 auth 解析接口，Anthropic OAuth 伪装身份头与工具名映射已随 anthropic_messages.rs 一并移植）
- image generation 子系统（T13）

## 开发要点

- 移植以上游 vitest 行为为金标准，逐适配器核对事件序；禁止「看起来像」式实现（可行性 §3.1 风险）
- compat 检测矩阵用表驱动表达（设计文档 §13 开放项），T03 先落 anthropic/openai/deepseek 等本任务所需子集，结构须支持 T13 全量扩展
- 每个适配器文件头部标注溯源注释（编码规范 §14.3）
- HTTP 用 `reqwest`（rustls）；SSE 解析与上游帧边界语义对齐（含 `parseJsonWithRepair`）
- live 测试用 `PIR_LIVE_TEST=1` 门禁，默认跳过（编码规范 §12.6）

## 进度跟踪

- [x] 设计细化
- [x] 实现
- [x] 自测
- [x] 门禁验收
- [x] 文档回写

## 自测清单

- [x] 上游 `packages/ai` 相关 vitest 用例意图移植为同名 Rust 测试并通过
- [x] 三适配器契约测试：固定请求 → 期望事件序列（录制/构造的 SSE 流驱动）——`crates/pir-ai/tests/contract_adapters.rs`（脚本化本地 HTTP 服务器 + 录制 SSE）
- [x] content_index 交错事件：消费者按 index 正确关联（非连续 block 顺序）——`test_responses_interleaved_content_index`
- [x] stream 不抛出：HTTP 错误 / 流中断 / abort / 校验失败 → 全部 Error 事件 + stopReason，无 panic、无 Err——`test_stream_does_not_throw_on_http_error` + 各适配器 `stream_simple_missing_auth_is_stream_error`、处理器错误事件单测
- [x] partial tool-call JSON：分片任意切分下累积结果一致；非法 JSON 结束时按上游语义报错——`utils/json_parse.rs` 与各处理器单测
- [x] compat 矩阵：检测值命中、部分覆盖回落检测值、显式覆盖优先——`openai_completions.rs` `detect_compat`/`get_compat` 表驱动单测
- [x] thinking 映射：各级别对三协议的选项映射正确（含 off=省略、xhigh 降 high）——`models.rs` clamp 单测 + 三适配器 `build_params` 单测
- [x] maxTokens 钳制与 thinking 预算默认表数值正确——`models.rs` / `utils/estimate.rs` 单测
- [x] transform_messages：孤儿 tool call / error 不回放 / 图片占位符各用例——`utils/transform_messages.rs` 单测
- [x] 两层重试：60s 上限立即失败、retry-after 解析、错误分类表命中——`utils/provider_retry.rs` / `utils/retry.rs` 单测 + 契约 `test_retry_then_success`
- [x] usage/cost 累计与上游字段语义一致（含 tiers、cacheWrite1h）——`utils/cost.rs` 单测 + 三适配器 usage 断言

## 门禁验收

通用门禁 G1–G7 全过。

任务特有标准：

- [x] 3 个适配器契约测试全过，事件类型序列与上游测试语义一致——`contract_adapters.rs`：`test_anthropic_messages_contract` / `test_openai_completions_contract` / `test_openai_responses_contract`（请求方法/路径/头/body + 事件类型序列双向断言）
- [x] live smoke：豁免——本环境无 `ANTHROPIC_API_KEY` / `OPENAI_API_KEY`；`tests/live_smoke.rs` 已实现 `PIR_LIVE_TEST=1` 门禁（未设时即时返回通过），模型/基址可用 `PIR_LIVE_*_MODEL` / `PIR_LIVE_*_BASE_URL` 覆盖
- [x] `Models::stream` 按 `model.api` 正确分发，未注册 api 报明确错误——`models.rs`：`test_models_stream_unknown_provider_stream_error`、`test_create_provider_missing_api_stream_error`
- [x] 需求 §5.5 横切条目（本任务范围内）逐条核对有测试锚点——见 §自测清单 各行锚点；范围外条目（Codex WS/代理/transport/image generation/Faux）归 T02/T13

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| D-004 | ApiStream trait 形状 → ProviderStreams（同步返回事件流） | 已回写 |
| D-005 | reqwest 直连替代官方 SDK 的可观测差异 | 已回写 |
| D-006 | 校验/解析层差异（jsonschema、models.json serde、措辞） | 已回写 |
| D-007 | sanitize_surrogates 恒等 | 已回写 |

## 验收记录

- 验收日期：2026-07-30
- 验收人：实现者自验（单人开发，按 gates.md §1 逐项自证）
- G1 构建/静态检查：通过（`cargo build --workspace`、`cargo clippy --workspace --all-targets -- -D warnings` 无警告、`cargo fmt --all -- --check` 干净）
- G2 测试：通过（`cargo test --workspace` 16 个测试目标全绿；pir-ai 242 lib + 7 contract + 3 live；live 未设 `PIR_LIVE_TEST=1` 默认跳过且通过；非 live 测试不访问真实网络——契约测试用 127.0.0.1 脚本化服务器）
- G3 对拍：部分适用——本任务不涉及 session JSONL / RPC / compaction / keybindings / tmux 等逐条对拍级基准（需求 §11.1），无 fixtures 对拍对象；事件序与线格式契约以「上游 vitest 意图移植 + 录制 SSE 契约测试」锚定（`tests/contract_adapters.rs` 7 例），错误文案不参与对拍（fixtures/README §2 粒度）
- G4 红线：通过（`external/pi` `git status --porcelain` 为空、HEAD=2efa728；无 JS 执行能力；未读写 `~/.pi`/`.pi`；仅 JSONL；token 估算 chars/4 未偏离；非测试代码 unwrap/expect 均带 invariant 注释；凭据不进 Debug（`StreamOptions` 手动 Debug 脱敏）/日志；无范围排除项引入；无外部 rg/fd；session 写入无文件锁）
- G5 线格式：通过（types/models.json 全部 `rename_all = "camelCase"` 或逐字段 rename；`Model`/`StreamEvent`/compat 形状有 serde 快照单测；契约测试断言请求 body 键序与键名）
- G6 文档同步：通过（全部移植文件头部溯源注释（`Port of ... @ pi 0.82.1 (2efa728)` + intentional differences）；回写 `02-design.md` §3.3/§3.4/§3.6/§12、`01-requirements.md` §5.5）
- G7 偏离闭环：通过（D-004～D-007 登记并回写；均实现细节级，无需 ADR）
- 结论：通过
