# T13：全量 Provider 与 OAuth

- **状态**：未开始
- **里程碑**：M6
- **依赖**：T03、T04
- **上游对照**：`packages/ai/src/api/*`（剩余 7 种）、`packages/ai/src/providers/*`（38 工厂）、`packages/ai/src/auth/*`（剩余 6 OAuth + provider 自有 login）、`docs/providers.md`、`docs/models.md`、`docs/custom-provider.md`、`packages/coding-agent/src/core/remote-catalog-provider.ts`
- **需求章节**：§5（全章）
- **预估**：3–4 人月

---

## 目标

补齐全部 KnownApi 适配器与 Provider 列表、剩余 OAuth 流程与横切能力，
达到需求 §5 全量覆盖（M6 验收口径）。

## 范围

### In

- 剩余 7 种 KnownApi 适配器（行为锚点见需求 §5.2）：
  - `azure_openai_responses`（base URL 归一化、deployment name map、API version 默认 v1；options 新增 6 字段 reasoningEffort/reasoningSummary + azureApiVersion/azureResourceName/azureBaseUrl/azureDeploymentName；host 3 后缀 .openai.azure.com/.cognitiveservices.azure.com/.ai.azure.com，命中时路径归一化 /openai/v1，azure-openai-responses.ts:56-63,190-193）
  - `openai_codex_responses`（**WebSocket 子系统**：连接缓存 5min/55min、per-session SSE 永久回退、两类一次重试、**缓存续传**（基线前缀校验后计算 input delta，发送 `{previous_response_id, input: delta}`，openai-codex-responses.ts:1387-1426）、debug stats、session 资源清理；SSE 路径 **zstd 压缩**；`chatgpt-account-id` JWT 解析；`originator` 字面值必须是 `"pi"`（openai-codex-responses.ts:1593）；`store:false`+`instructions`+encrypted_content+verbosity；service tier 价格乘数）
  - `google_generative_ai`（模型族 thinking 分流：Gemini 3/Gemma 4 thinkingLevel 不可关 vs 其余 budget；**budget 档位表**（minimal/low/medium/high：2.5-pro=128/2048/8192/32768、2.5-flash-lite=512/2048/8192/24576、2.5-flash=128/2048/8192/24576、其余 -1 动态；`options.thinkingBudgets` 优先）；**usage 映射**（input=promptTokenCount−cachedContentTokenCount、output=candidatesTokenCount+thoughtsTokenCount、cacheRead=cachedContentTokenCount、cacheWrite=0、reasoning=thoughtsTokenCount）（google-generative-ai.ts:218-236,469-508）；thoughtSignature 保留；VALIDATED；无函数调用流式；id 自增）
  - `google_vertex`（API key/ADC、project/location 解析；baseUrl 含 `{location}` 占位符时被整体丢弃、回退 SDK 默认端点，**不做模板替换**，google-vertex.ts:391-397）
  - `bedrock_converse_stream`（SigV4 vs bearer、region 解析顺序、header 白名单、cachePoint 1h、interleaved thinking；EMPTY_TEXT_PLACEHOLDER = "<empty>"（三处空白文本占位）；自适应 thinking 模型族子串清单 opus-4-6/4-7/4-8/5、sonnet-4-6/5、fable-5（归一化小写、`[\s_.:]+`→`-`）；xhigh 族为其子集去掉 opus-4-6、sonnet-4-6（bedrock-converse-stream.ts:104,573-605））
  - `mistral_conversations`（promptMode vs reasoningEffort、id 9 字符归一化、x-affinity/promptCacheKey、cached tokens 6 种字段名变体：promptTokensDetails.cachedTokens、prompt_tokens_details.cached_tokens、promptTokenDetails.cachedTokens、prompt_token_details.cached_tokens、numCachedTokens、num_cached_tokens；钳制 [0, promptTokens]，mistral-conversations.ts:278-296）
  - `pi_messages`（/messages 端点、rewrite 诊断、debug=1）
- **compat 检测矩阵全量数据**（T03 基础设施上扩展至全部 provider：zai/together/moonshot/openrouter/cloudflare/nvidia/ant-ling/cerebras/xai/chutes/deepseek/opencode 等）；OpenRouter 路由偏好全字段、grammar tools（lark/regex）、zaiToolStream、Kimi deferred tools
- **Provider 全集 38 个内置工厂**（需求 §5.3 清单，含区域拆分：zai/zai-coding-cn、minimax(+cn)、moonshotai(+cn)、xiaomi 四端点、qwen-token-plan(+cn)、cloudflare 两个、opencode/opencode-go、radius）；**混合 API provider 按 `model.api` 分发**（github-copilot 3 API + `filterModels`、opencode 4 API 等）；cloudflare 占位符物化与 `cf-aig-authorization`
- 剩余 6 个 OAuth 流程：openai-codex（PKCE、`id_token_add_organizations`、originator；**device-code 旁路**：OpenAI 私有 deviceauth 端点 `/api/accounts/deviceauth/usercode|token`，验证 URI `/codex/device`，openai-codex.ts:31-37,277）、github-copilot（**device code**、enterprise 域名、per-account baseUrl、`availableModelIds` 过滤；**登录后 policy-enable 步骤**：对每个已知模型 POST `${baseUrl}/models/{id}/policy`，body `{state:"enabled"}`，头 `openai-intent: chat-policy`，github-copilot.ts:294-327,353-354）、openrouter（PKCE 换**永久 key**，refresh no-op）、kimi-coding、xai、radius；**device code 流程共 5 家**：github-copilot/kimi-coding/xai/radius（RFC 8628）+ openai-codex（deviceauth 变体）
- provider 自有 login：Bedrock（bearer-token/aws-profile/credential-chain）、Vertex（api-key/adc/service-account）、Cloudflare（多字段 prompt 存 `credential.env`）
- 横切能力收尾：cross-provider handoff 全规则（T03 基础上的 provider 特异分支）、transport 偏好（仅 codex 实现 WS，其余静默忽略）、image generation 子系统（`ImagesModels`/`ImagesProvider`，OpenRouter images 非流式 modalities，永不 reject）、usage 兜底变体（Moonshot `choice.usage`、Mistral cached 字段）、deferred tools 各协议回退（Anthropic tool_reference 排除规则 / Kimi 序列化 / OpenAI tool_search）
- **远程 catalog overlay**：`pir update --models`（ETag/If-None-Match、**4 小时新鲜度**、generatedAt 比对；15s 超时；endpoint 可配置 ADR-0002 §8）+ `ModelsStore` 完整化（`refresh({allow_network:false})` 离线恢复、`force`、inflight 去重）
- 内置模型目录全量注册（`build.rs` 生成管线正式化：models.dev 数据源 + provider 修正规则）；按需注册子集机制（feature flags）

### Out

- llama.cpp 的产品化集成（`/llama`、本地模型管理，T14）；本任务只含 llama.cpp router 动态 provider 机制
- 产品 endpoint 配置化（T14）

## 开发要点

- 适配器多数为薄封装，难点在鉴权差异与 catalog 同步（可行性 §3.1）；每适配器仍需契约测试，不得因「薄」省略
- Bedrock 已钉死：手写 SigV4 + reqwest + 自实现 event-stream 解码，**不引** `aws-sdk`（设计文档 §14）
- **Google/Bedrock 来源空白注记**：两适配器上游分别委托 `@google/genai` 与 `@aws-sdk`，传输层细节（基址/SSE/SigV4/event-stream）TS 源码不可考，须从 SDK 行为与 AWS 规范反推
- Codex WS 状态机的 Rust 表达是设计文档 §13 开放项，本任务定稿并回写
- catalog 刷新遵循「生成数据只读、修改走生成器」（编码规范 §3.2）
- live 测试矩阵按 provider 分组，`PIR_LIVE_TEST=1` 门禁，CI 默认不跑

## 进度跟踪

- [ ] 设计细化
- [ ] 实现
- [ ] 自测
- [ ] 门禁验收
- [ ] 文档回写

## 自测清单

- [ ] 10 种 KnownApi 适配器契约测试全过（每适配器至少：正常流、thinking、tool-call、错误流）
- [ ] Provider 注册表 38 工厂与上游逐条核对（id、默认 base URL、auth 方式、混合 API 分发）
- [ ] compat 矩阵全量命中测试（各 provider/baseUrl → 期望默认值）
- [ ] 各 OAuth 流程单测（mock 授权端点）：PKCE / device code / refresh / openrouter 永久 key 特例
- [ ] Codex WS：连接缓存 TTL、SSE 回退、重试规则、zstd 请求体（contract 测试）
- [ ] handoff：跨 provider 会话切换的消息转换与上游一致
- [ ] transport 偏好设置生效与回退语义
- [ ] catalog 刷新：`pir update --models` 产出与加载链路可用；ETag/新鲜度/离线恢复
- [ ] image generation：OpenRouter images 契约测试（永不 reject）

## 门禁验收

通用门禁 G1–G7 全过。

任务特有标准：

- [ ] 需求 §5 全章逐条核对有锚点（验收记录列映射表）
- [ ] live smoke 矩阵：有 key 的 provider 各完成一次真实调用（结果记录；无 key 的记录豁免）
- [ ] 上游 `packages/ai` providers/auth 相关测试意图移植清单完成

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| — | （暂无） | — |

## 验收记录

（待填写，模板见 `gates.md` §3）
