# T13：全量 Provider 与 OAuth

- **状态**：未开始
- **里程碑**：M6
- **依赖**：T03、T04
- **上游对照**：`packages/ai/src/api/*`（剩余 7 种）、`packages/ai/src/providers/*`、`packages/ai/src/auth/*`（剩余 OAuth 流程）
- **需求章节**：§5（全章）
- **预估**：3–4 人月

---

## 目标

补齐全部 KnownApi 适配器与 Provider 列表、剩余 OAuth 流程与横切能力，
达到需求 §5 全量覆盖（M6 验收口径）。

## 范围

### In

- 剩余 7 种 KnownApi 适配器：`azure_openai_responses`、`openai_codex_responses`、`google_generative_ai`、`google_vertex`、`bedrock_converse_stream`、`mistral_conversations`、`pi_messages`
- Provider 全集（对齐 Pi README 列表，需求 §5.2）：Anthropic、OpenAI、Azure、Codex、DeepSeek、Google/Vertex、Bedrock、Mistral、Groq、Cerebras、xAI、OpenRouter、Vercel AI Gateway、Cloudflare、GitHub Copilot、ZAI、MiniMax、Kimi、Hugging Face、Fireworks、Together、OpenCode、Xiaomi MiMo、NVIDIA NIM、Ant Ling、llama.cpp router、任意 OpenAI-compatible
- 剩余 OAuth 流程：Codex、GitHub Copilot 等（各 provider 独立模块）
- 横切能力：cross-provider handoff、transport 偏好（`sse|websocket|websocket-cached|auto`）、image generation（OpenRouter images 等）、远程 catalog 刷新（`pir update --models`，生成数据管线正式化）
- 内置模型目录全量注册；按需注册子集机制（feature flags）

### Out

- llama.cpp 的产品化集成（`/llama`、本地模型管理，T14）；本任务只含 llama.cpp router provider
- 产品 endpoint 配置化（T14）

## 开发要点

- 适配器多数为薄封装，难点在鉴权差异与 catalog 同步（可行性 §3.1）；每适配器仍需契约测试，不得因「薄」省略
- Bedrock 已钉死：手写 SigV4 + reqwest + 自实现 event-stream 解码，**不引** `aws-sdk`（设计文档 §14）
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
- [ ] Provider 注册表与上游列表逐条核对（名称、默认 base URL、auth 方式）
- [ ] 各 OAuth 流程单测（mock 授权端点）：PKCE / device code / refresh
- [ ] handoff：跨 provider 会话切换的消息转换与上游一致
- [ ] transport 偏好设置生效与回退语义
- [ ] catalog 刷新：`pir update --models` 产出与加载链路可用

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
