# T20：Provider 适配器修复与 compat 扩展

- **状态**：未开始
- **里程碑**：M2
- **依赖**：T17
- **上游对照**：`59ad3dead`（Anthropic #7358）、`6138f5a07`+`cbaca6038`+`b9d360a2c`（Google #7362/#7494/#7471）、`9dd90a497`（Mistral 原生传输）、`cfe6b6a05`（Codex WS #7364）、`c185d4123`+`2fe21b407`+`4c1a0b92e`（completions 请求体）、`25a2c8dcf`（samplingParams #7568）、`d07889da0`（thinking_token_budget #7638）、`e47b8e37a`（additional_tools #7709）、`c3e7bc60a`（endTurn #7766）、`027a58479`（fetch 注入）、`0c32e83a3`（llama usage）、`70bbe47a9`（Bedrock 错误元数据 #7286）；测试：`mistral-http-transport.test.ts`（427 行）、`google-shared-signed-empty-blocks.test.ts`、`google-shared-retry.test.ts`、`sampling-options.test.ts`、`fetch-option.test.ts`
- **需求章节**：v0.11 需求 R2.1.3–R2.1.4（行为部分）、R2.4.1–R2.4.4、R2.4.8–R2.4.11、R2.7.4；设计 §2.2、§2.3
- **预估**：0.6 人月

---

## 目标

落地各 provider 适配器的流式细节修复与 OpenAI-completions/Responses 的 compat 能力扩展，
含 Mistral SDK → 原生 SSE 传输替换。

## 范围

### In

- **Anthropic**：`content_block_start` 的 text/thinking 初始内容不再丢弃
- **Google**：带 thought signature 的空 text/thinking block 保留（无签名空块仍丢弃）；`requiresToolCallId()` 扩展到 Gemini 3.x+；GenAI 错误（408/409/429/5xx + retry-after）纳入统一重试（`retryGoogleRequest()` 等价）
- **Mistral**：删除 SDK 依赖改原生 SSE（自解析 `data:`/`[DONE]`/多行 JSON）；`to_mistral_wire_payload()` camelCase→snake_case 映射；`x-affinity` 头保留；以 `mistral-http-transport.test.ts` 为对拍蓝本
- **Codex**：WS session 缓存改 `sessionId → accountId` 二级 Map；`endTurn` 从 `response.done/completed/incomplete` 提取
- **completions 请求体**：`use_max_tokens` 名单 +DeepSeek（provider 或 baseUrl 含 deepseek.com）+Z.AI；qwen thinking 分支 `reasoning_effort` 映射；`sampling_params` 请求体**最后**合并（键覆盖命名参数）；vLLM `supportsThinkingTokenBudget`（档位预算 minimal 1024/low 2048/medium 8192/high 16384 可覆盖 + `MIN_ANSWER_TOKENS = 1024` 预留）
- **Responses**：`deferredToolsMode: "additional-tools" | "tool-search"`，GPT-5.6 系优先消息锚定 `additional_tools` 输入项；`namespace` 回放规则（同模型或已加载 deferred 工具）
- **Bedrock**：失败时向 `diagnostics` 追加 `{type:"bedrock_response_failure", details:{status,errorCode,requestId}}`（requestId ≤200 字符）
- **llama.cpp**：`supportsUsageInStreaming: true`
- **每请求 fetch 注入**通道接线（T17 的类型落地到各适配器）；Google 适配器拒绝非默认 fetch
- compat 字段（`chatTemplateArgs`/`supportsFinishReason`/`supportsThinkingTokenBudget`/`supportsAdditionalTools`）的 models.json/schema 透传

### Out

- Baseten / Qwen Token Plan Individual 新 provider 与目录生成管线更新（T26）
- GLM 5.2 / Kimi K3 / Copilot 路由修正（T26 目录侧）
- deferred 请求生命周期（[DEFER]）

## 开发要点

- Mistral 是最大块：先以 427 行上游测试移植锁定行为，再删 SDK 路径
- `sampling_params` 合并顺序是行为契约（最后合并、键可覆盖），黄金用例锁死
- compat 新字段贯穿 `models.json` schema → 工厂 → 适配器三层，逐层核对

## 进度跟踪

- [ ] 设计细化
- [ ] 实现
- [ ] 自测
- [ ] 门禁验收
- [ ] 文档回写

## 自测清单

- [ ] Anthropic 初始块内容 golden（含 thinking 块）
- [ ] Google：签名空块保留 / 无签名丢弃 / Gemini 3+ tool call id / 重试归一化（retry-after）四组
- [ ] Mistral 原生传输 427 行测试意图全移植通过；SDK 依赖从 Cargo.toml 移除
- [ ] Codex 双账号不共享 WS 连接用例；`endTurn` 提取 golden
- [ ] completions：DeepSeek/Z.AI `max_tokens`、qwen `reasoning_effort`、`sampling_params` 末位合并、thinking_token_budget 档位 + 1024 预留各 golden
- [ ] Responses additional_tools 模式 + namespace 回放规则 golden
- [ ] Bedrock diagnostics 元数据、llama streaming usage、fetch 注入（Google 拒绝分支）各回归

## 门禁验收

通用门禁 G1–G7 全过（G3 强制；G2 附期望修改清单）。

任务特有标准：

- [ ] 需求 R2.4 十一条逐条核对表（每条上游 commit + pir 测试锚点）
- [ ] Mistral 依赖移除后 `Cargo.lock` 审计（无残留传递依赖）

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| （待登记） | | |

## 验收记录

（按 gates §3 模板填写）
