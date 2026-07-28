# T03：pir-ai 核心协议（Anthropic + OpenAI 系）

- **状态**：未开始
- **里程碑**：M1
- **依赖**：T01（契约测试基建依赖 T02 的部分可并行接入）
- **上游对照**：`packages/ai/src/api/{anthropic-messages,openai-completions,openai-responses}.ts`、`packages/ai/src/types.ts`
- **需求章节**：§5.1（3 种 KnownApi）、§5.3（thinking / usage / cost / image input）
- **预估**：2.5–3 人月（M1 共 3–4，与 T04 合计）

---

## 目标

实现 `pir-ai` 的类型层完备化与首批 3 个 API 适配器，打通「Context → 协议流式事件」
的核心链路，供 M2 的 agent loop 与后续 headless 模式使用。

## 范围

### In

- `ApiStream` trait 与 `Models::stream` / `stream_simple` 分发（设计文档 §3.3）
- 适配器（文件名与上游一一对应）：
  - `api/anthropic_messages.rs`
  - `api/openai_completions.rs`
  - `api/openai_responses.rs`
- 流式增量解析：text / thinking 增量透传；partial tool-call JSON 流式累积 + 结束校验（编码规范 §7.2）
- thinking 级别统一映射（`off|minimal|low|medium|high|xhigh|max` + `thinkingBudgets`）在 `stream_simple` 层
- usage / cost / cache 统计类型与累计逻辑
- 工具参数 JSON Schema 校验（`jsonschema`）
- `models.json` 加载与内置模型注册机制；生成数据管线（`build.rs` 或更新命令占位，正式刷新在 T13/T14）
- image input（base64 data block）支持
- 错误处理：`AiError` → `StreamEvent::Error` + `stopReason`（编码规范 §5.2）

### Out

- 其余 7 种 KnownApi 与全量 provider（T13）
- OAuth 流程与凭据存储（T04；本任务只定义 auth 解析接口）

## 开发要点

- 移植以上游 vitest 行为为金标准，逐适配器核对事件序；禁止「看起来像」式实现（可行性 §3.1 风险）
- 每个适配器文件头部标注溯源注释（编码规范 §14.3）
- HTTP 用 `reqwest`（rustls）；SSE 解析与上游帧边界语义对齐
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
- [ ] partial tool-call JSON：分片任意切分下累积结果一致；非法 JSON 结束时按上游语义报错
- [ ] thinking 映射：各级别对三协议的选项映射正确
- [ ] usage/cost 累计与上游字段语义一致
- [ ] 错误路径：HTTP 错误 / 流中断 → `StreamEvent::Error`，无 panic

## 门禁验收

通用门禁 G1–G7 全过。

任务特有标准：

- [ ] 3 个适配器契约测试全过，事件类型序列与上游测试语义一致
- [ ] live smoke（有 key 时）：Anthropic 与 OpenAI 各完成一次真实流式调用（结果记录；无 key 则记录豁免理由）
- [ ] `Models::stream` 按 `model.api` 正确分发，未注册 api 报明确错误

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| — | （暂无） | — |

## 验收记录

（待填写，模板见 `gates.md` §3）
