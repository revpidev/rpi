# T19：流终止语义与通用流修复

- **状态**：未开始
- **里程碑**：M2
- **依赖**：T17
- **上游对照**：`23cb385b6`/`926eb15c1`/`637737ca7`/`fe1c9b6d5`/`e5ef8d065`/`5a53f086e`（rawStopReason 系列，merge `d7b02636a`）、`2c3041242`（supportsFinishReason）、`32850ef7c`（isRecoverableLength）、`34239180a`+`f9476a61e`（tool-call delta）、`2e95584da`（union 校验）、`4523528b2`（错误体）、`fe10558eb`（重试文案）；测试：`openai-responses-terminal-event.test.ts`、`*-raw-stop-reason.test.ts`
- **需求章节**：v0.11 需求 R2.3、R2.4.5–R2.4.7；设计 §2.2
- **预估**：0.4 人月

---

## 目标

对齐上游 v0.83–v0.84 的流终止语义：未映射 reason 错误化、rawStopReason 全 provider 覆盖、
可恢复截断判定，以及与 provider 无关的通用流修复。这是对拍差异最高发区，逐条配 golden。

## 范围

### In

- **stop-reason 映射返回结构改为 `(StopReason, Option<error_message>)`**（各 provider 映射函数签名统一调整）：
  - Anthropic `sensitive` → `error` + `"Provider stopped with: sensitive"`
  - Bedrock 未知 reason → `"Provider stopped with: <reason>"`
  - Mistral 未知 finish reason → `"Mistral stopped with: <reason>"`
  - OpenAI Responses：`incomplete_details.reason == "max_output_tokens"` 是 length 的**唯一**来源；`content_filter`/`max_time_limit` 等 → `error` + `"Response incomplete: <reason>"`
- `rawStopReason` 填充：Anthropic/Google/Bedrock/Mistral/OpenAI completions/responses 六家族（responses 的 raw 值形如 `"completed"`/`"incomplete.max_output_tokens"`）
- `supportsFinishReason` compat：无 `finish_reason` 流按内容推断 `toolUse`/`stop`；声明支持却缺失时报 `"Stream ended without finish_reason"`
- `is_recoverable_length()`（`stopReason == Length && output < desiredMaxOutput`），供 T23 恢复链消费
- `aborted`/`error` 时错误消息携带 `output.errorMessage`（不固定 "An unknown error occurred"）
- 通用修复：tool-call delta（合法 `function` + 空 `custom` 不丢参数）、union 校验先匹配原值分支（nullable 不强转）、错误体仅 plain object 结构化、可重试文案新增 `"exceeded request buffer limit while retrying upstream"`

### Out

- provider 专属修复（Anthropic 初始块内容、Google 签名空块/重试、Mistral 传输、Codex 缓存 → T20）
- length-stop 的 compact-and-retry 恢复链（T23）
- compat 新标志的目录生成接线（T26）

## 开发要点

- 每条修复 = 一个 golden 用例，期望值直接移植上游测试（蓝本：`openai-responses-terminal-event`、`mistral-raw-stop-reason`、`bedrock-raw-stop-reason`、`openai-completions-raw-stop-reason`）
- 映射函数签名变更会带出所有 provider 调用点，逐点核对而非机械适配
- 注意与 v0.1 既有 stop-reason 测试的期望差异，全部走 G2「旧期望 → 新期望」清单

## 进度跟踪

- [ ] 设计细化
- [ ] 实现
- [ ] 自测
- [ ] 门禁验收
- [ ] 文档回写

## 自测清单

- [ ] 未映射 reason 错误化：Anthropic/Bedrock/Mistral/Responses 四家族 golden（文案逐字节）
- [ ] rawStopReason 六家族 golden（含 responses 的 `incomplete.max_output_tokens` → length + raw 保留）
- [ ] supportsFinishReason 两分支（推断 / 报错）golden
- [ ] `is_recoverable_length` 边界（output == desiredMaxOutput 不恢复）
- [ ] tool-call delta / union nullable / plain-object 错误体 / 新重试文案四条通用修复各配回归

## 门禁验收

通用门禁 G1–G7 全过（G3 强制：逐条 golden；G2 附期望修改清单）。

任务特有标准：

- [ ] 上游 `rawStopReason` commit 链（7 个）与修复 commit（4 个）逐条有 rpi 测试锚点
- [ ] 需求 R2.3/R2.4.5–R2.4.7 逐条核对表

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| （待登记） | | |

## 验收记录

（按 gates §3 模板填写）
