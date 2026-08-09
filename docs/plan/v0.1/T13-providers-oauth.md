# T13：全量 Provider 与 OAuth

- **状态**：已完成
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
- **远程 catalog overlay**：`rpi update --models`（ETag/If-None-Match、**4 小时新鲜度**、generatedAt 比对；15s 超时；endpoint 可配置 ADR-0002 §8）+ `ModelsStore` 完整化（`refresh({allow_network:false})` 离线恢复、`force`、inflight 去重）
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
- live 测试矩阵按 provider 分组，`RPI_LIVE_TEST=1` 门禁，CI 默认不跑

## 进度跟踪

- [x] 设计细化
- [x] 实现（W1–W6 六波次全部落地，2026-08-06）
- [x] 自测（3404 测试全绿；W7 重跑门禁）
- [x] 门禁验收（G1–G7 全过，编排方复核通过，2026-08-07）
- [x] 文档回写（W1–W6 各波次已回写；W7 核对并修正 `02-design.md` §12 一处路径矛盾）

## 设计细化记录（2026-08-06）

现状盘点（T03/T04 已落地）：

- 适配器 3/10：`anthropic_messages` / `openai_completions` / `openai_responses`（+ `sse`/`lazy`/`simple_options`/`copilot_headers`/`openai_prompt_cache`/`constrained_sampling` 共享件）
- OAuth 1/7：anthropic（pkce/device_code/callback_page 基建已有）
- `models.rs` 注册设施（`create_provider`/`Models`/`ProviderApi::Map` 混合分发）已就绪；**无 provider 工厂**；`build.rs` 为占位空目录
- `types.rs` 10 个 KnownApi 常量已锁定；`ModelCompat` 平铺结构已在 models_json 使用

工作波次（每波独立可验收、独立提交）：

| 波次 | 子交付 | 上游锚点 | 产出文件（`crates/rpi-ai/`） |
|------|--------|----------|------------------------------|
| W1 | 适配器批 1：pi-messages、mistral-conversations、google-generative-ai（+google-shared） | `api/pi-messages.ts`、`api/mistral-conversations.ts`、`api/google-generative-ai.ts`、`api/google-shared.ts` | `src/api/pi_messages.rs`、`src/api/mistral_conversations.rs`、`src/api/google_generative_ai.rs`、`src/api/google_shared.rs` + 各自契约测试 |
| W2 | 适配器批 2：azure-openai-responses、google-vertex、bedrock-converse-stream（手写 SigV4 + event-stream，ADR 钉死不引 aws-sdk） | `api/azure-openai-responses.ts`、`api/google-vertex.ts`、`api/bedrock-converse-stream.ts` | `src/api/azure_openai_responses.rs`、`src/api/google_vertex.rs`、`src/api/bedrock_converse_stream.rs`（+`api/bedrock/` sigv4、eventstream 子模块）+ 契约测试 |
| W3 | openai-codex-responses（WS 子系统：连接缓存 5min/55min、SSE 回退、缓存续传、zstd、JWT chatgpt-account-id、originator="pi"） | `api/openai-codex-responses.ts` | `src/api/openai_codex_responses.rs`（+`api/codex_ws/` 子模块）+ 契约测试；§13 开放项定稿回写设计文档 |
| W4 | 38 Provider 工厂 + compat 矩阵全量 + 内置模型目录生成管线（`build.rs` 正式化：上游 `providers/data/*.json` 30 份 + `*.models.ts` 修正规则）；混合 API 分发（copilot 3 API/opencode 4 API） | `providers/*.ts`、`providers/all.ts`、`providers/data/` | `src/providers/`（每 provider 一文件）、`src/providers/data/*.json`（vendored 只读）、`build.rs` 生成器、`compat` 全量数据 |
| W5 | 6 OAuth 流程（openai-codex 含 deviceauth 旁路、github-copilot 含 policy-enable、openrouter 永久 key、kimi-coding、xai、radius）+ provider 自有 login（bedrock/vertex/cloudflare）+ `load.ts` 对应物 | `auth/oauth/*.ts`、`providers/*-auth.ts` | `src/auth/oauth/{openai_codex,github_copilot,openrouter,kimi_coding,xai,radius}.rs`、`src/auth/oauth/load.rs`、provider login 入 `src/providers/` |
| W6 | 横切收尾：cross-provider handoff provider 特异分支、transport 偏好、images 子系统（ImagesModels/ImagesProvider、openrouter-images 永不 reject）、usage 兜底变体、deferred tools 各协议回退；远程 catalog overlay（`rpi update --models`、ETag/4h 新鲜度、ModelsStore 完整化） | `utils/transform-messages.ts`、`providers/images/`、`openrouter-images.ts`、`coding-agent/src/core/remote-catalog-provider.ts` | `src/images/`、`src/api/openrouter_images.rs`、`src/models_store.rs` 补全、`crates/rpi` update 子命令 |
| W7 | 上游测试意图移植清单（`packages/ai` 根目录 ~110 测试文件中 providers/auth/api 相关）、需求 §5 逐条映射表、live smoke 矩阵记录、门禁验收 | `packages/ai/*.test.ts` | `crates/rpi-ai/tests/` 扩充 + 任务文件验收记录 |

约定：

- 每适配器契约测试四件套（正常流/thinking/tool-call/错误流）不可省；mock HTTP 用现有 `rpi-test-support` 设施，风格对齐 `tests/contract_adapters.rs`
- Google/Bedrock 传输层为来源空白（上游委托 SDK），按 SDK 公开行为与 AWS/Google 公开规范反推，登记偏离
- 偏离一事一记（`deviations/`），行为级先立 ADR；catalog 生成数据只读、修改走生成器（编码规范 §3.2）
- live 测试 `RPI_LIVE_TEST=1` 门禁，默认不跑
- 各波次共享 `api.rs`/`lib.rs` 注册点：模块行按字母序插入，编辑前重读防冲突

## 自测清单

- [x] 10 种 KnownApi 适配器契约测试全过（每适配器至少：正常流、thinking、tool-call、错误流）——`tests/contract_adapters.rs`（7）+ `contract_pi_messages.rs`（9）+ `contract_mistral_conversations.rs`（7）+ `contract_google_generative_ai.rs`（33）+ `contract_google_vertex.rs`（23）+ `contract_azure_openai_responses.rs`（10）+ `contract_bedrock_converse_stream.rs`（31）+ `contract_openai_codex_responses.rs`（13），W7 全量重跑通过
- [x] Provider 注册表 38 工厂与上游逐条核对（id、默认 base URL、auth 方式、混合 API 分发）——`tests/providers_group_a.rs`（15）+ `group_b`（11）+ `group_c`（8）+ `group_d`（12）共 46 用例；`providers.rs` spec 表 `factory: Some` × 38（无占位）
- [x] compat 矩阵全量命中测试（各 provider/baseUrl → 期望默认值）——`tests/compat_matrix.rs` 18 用例
- [x] 各 OAuth 流程单测（mock 授权端点）：PKCE / device code / refresh / openrouter 永久 key 特例——`tests/oauth_codex_openrouter.rs`（5）+ `oauth_copilot_radius.rs`（5）+ `oauth_kimi_xai.rs`（6）+ `auth_oauth_resolve.rs`（1）+ `device_code.rs`/`pkce.rs` 单测 10
- [x] Codex WS：连接缓存 TTL、SSE 回退、重试规则、zstd 请求体（contract 测试）——`contract_openai_codex_responses.rs` 13 用例（缓存续传 `test_websocket_cached_delta_and_reuse`、TTL `test_websocket_connection_cache_ttls`、永久回退 `test_websocket_idle_fallback_is_permanent`、两类重试 2 用例、zstd `test_sse_contract_headers_and_zstd_body`）
- [x] handoff：跨 provider 会话切换的消息转换与上游一致——`utils/transform_messages.rs` 单测 9 + `contract_adapters.rs:494`（completions 跨 provider id 归一化）
- [x] transport 偏好设置生效与回退语义——codex WS/SSE/auto 契约 5 用例；其余 provider 静默忽略（D-036 记录，与上游一致）
- [x] catalog 刷新：`rpi update --models` 产出与加载链路可用；ETag/新鲜度/离线恢复——`crates/rpi/src/core/remote_catalog_provider.rs` 单测 14（离线恢复/并发去重/abort/4h 常量）+ `package_command.rs` 单测 7 + `tests/model_catalog.rs` 4 用例
- [x] image generation：OpenRouter images 契约测试（永不 reject）——`tests/images.rs` 14 用例

## 门禁验收

通用门禁 G1–G7 全过。

任务特有标准：

- [x] 需求 §5 全章逐条核对有锚点（验收记录列映射表）
- [x] live smoke 矩阵：有 key 的 provider 各完成一次真实调用（结果记录；无 key 的记录豁免）——W7 检查环境变量：**无任何 provider key 存在**，全部豁免；`RPI_LIVE_TEST=1` 下 live_smoke 全部跳过不失败（结果见验收记录）
- [x] 上游 `packages/ai` providers/auth 相关测试意图移植清单完成（验收记录附表，114 文件逐文件标注）

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| D-021 | pi-messages 适配器 Rust 落地差异（toolChoice/debug 走私字段、SSE 文案 serde_json 化、statusText canonical reason、RPI_CACHE_RETENTION 不设默认、截断按 Unicode scalar 等） | 已回写 |
| D-022 | mistral-conversations 适配器落地差异（reqwest 直连、wire JSON、x-affinity 覆盖检查、重试共享 helper 默认 0 等） | 已回写 |
| D-023 | google-generative-ai `@google/genai` SDK 反推与 reqwest 直连差异（线格式反推、遥测头/重试缺失、chunk 级流内错误探针等） | 已回写 |
| D-024 | azure-openai-responses 适配器落地差异（openai@6.26.0 源码核对线格式、api-key 头、deployment 在 body 等） | 已回写 |
| D-025 | google-vertex SDK 反推与 ADC 子集自实现差异（URL 规则、global/多区域端点、external_account 显式报错缺口等） | 已回写 |
| D-026 | bedrock-converse-stream `@aws-sdk`/`@smithy` 反推差异（手写 SigV4、event-stream 帧解码、凭据链仅 env 缺口、新增 sha2/hmac 依赖） | 已回写 |
| D-027 | openai-codex-responses WS 状态机 Rust 表达（socket 移出 entry、spawn 代际计数、非阻塞 poll 探针、zstd 恒压缩、新增 tokio-tungstenite/zstd/libc 依赖） | 已回写 |
| D-028 | 内置模型目录管线与注册表骨架落地差异（include_str!+惰性解析、修正规则烘焙于 vendored JSON、37 JSON+manifest 非任务书 30 份） | 已回写 |
| D-029 | kimi-coding 工厂 OAuth 槽 W4 占位（W5 已接线） | 已关闭 |
| D-030 | openai-codex 工厂 auth W4 占位（W5 已填入 openai_codex_oauth） | 已关闭 |
| D-031 | xai 工厂 OAuth 槽 W4 占位（W5 已接线；同 D-029 模式） | 已关闭 |
| D-032 | providers group B 八工厂落地差异（filter_models 默认方法、PendingOAuth stub、cloudflare-auth 两 kind 合一等；W5 已解决 copilot/radius/openrouter OAuth，openrouter loginLabel 仍缺槽位） | 已回写 |
| D-033 | github-copilot / radius OAuth 流程落地差异（URL 重写测试缝、独立 axum 回调服务、ring UUIDv4 等） | 已回写 |
| D-034 | kimi-coding / xai OAuth 与 load.ts 对应物落地差异（构造字段测试缝、30s 超时、load.rs registry 函数表等） | 已回写 |
| D-035 | openai-codex / openrouter OAuth 落地差异（authority URL 重写、atob 四字母表、MAX_SAFE_INTEGER 精确值、refresh no-op 等） | 已回写 |
| D-036 | `cross-provider-handoff.test.ts` 为 live 测试不移植；意图由 transform_messages 全规则 + 六适配器 normalize 回调纯函数测试覆盖 | 已回写 |
| D-037 | image generation 子系统落地差异（文件聚合、ImagesApiKind newtype、惰性注册、目录 node 转写 40 模型、image-model-data.test.ts 意图以目录校验测试表达） | 已回写 |
| D-038 | 远程 catalog overlay 与 `Models::refresh` 落地差异（fetchModels 钩子延后 T15、refresh_models 以 Option<BoxFuture> 表达、loopback 测试服务器、15s 超时常量） | 已回写 |

## 验收记录

- 验收日期：2026-08-07（W7 收官波次；编排方复核通过，状态置「已完成」）
- 验收人：T13 W7（文档/核对波次）；最终复核由编排方执行
- G1 构建/静态检查：通过。命令输出摘要：
  ```
  $ cargo build --workspace        → Finished `dev` profile in 0.14s（无警告）
  $ cargo clippy --workspace --all-targets -- -D warnings → Finished（无警告）
  $ cargo fmt --all -- --check     → 通过（W7 新增 live 目标经 cargo fmt 后复检）
  ```
- G2 测试：通过（`cargo test --workspace` 全量 **3404 passed, 0 failed**；live 测试在未设 `RPI_LIVE_TEST=1` 时默认跳过且不得失败；`RPI_LIVE_TEST=1 cargo test -p rpi-ai --test live_smoke` 下 7 目标全部跳过通过）
- G3 对拍：适用（协议契约类）。T13 行为契约的载体为**契约测试**（与上游同构的 loopback mock-server wire 断言：请求路径/头/body、SSE 事件序、WS 状态机），共 8 个 `tests/contract_*.rs` 133 用例；模型目录对拍为 `tests/model_catalog.rs` 对 vendored JSON 的 sha256（manifest 逐字节）与字段级对拍。不属于 session-format/rpc/compaction/keybindings/tmux 逐条基准领域，无 fixtures 归一化 diff 需求。
- G4 红线：通过（逐条确认见下）
- G5 线格式：通过（新增 wire 类型均 camelCase serde 形状与上游逐个核对；vendored 目录 sha256 对拍 `test_vendored_files_match_manifest_sha256`）
- G6 文档同步：通过（W1–W6 各波次已回写 `02-design.md` §3.3/§3.4/§3.5/§3.6/§12/§13、`coding-standards.md` 附录 A、`01-requirements.md` §5.4/§5.5/§6.6 等；W7 核对并修正 `02-design.md` §12 一处路径矛盾（`remote_catalog.rs` → `remote_catalog_provider.rs`）与 §13 开放项收口（compat 矩阵数据化表达已定稿为表驱动））
- G7 偏离闭环：通过（D-021~D-038 共 18 条全部已登记；已回写 15 条 + 已关闭 3 条（D-029/030/031，W5 接线后关闭）；行为级 0 条；关联 ADR：无新增（Google/Bedrock SDK 反推属实现细节偏离，逐条回写设计文档 §3.3））
- 结论：通过（任务特有标准三项——§5 映射表、live smoke 矩阵、上游测试移植清单——均完成；**状态置「待验收」**，最终验收由编排方复核后标记「已完成」）

### G4 红线逐条确认

- [x] `external/pi/` 无任何改动：`git status --porcelain` 为空，HEAD = `2efa728d2ee90ef597626e96b1e28ef2b279f07c`
- [x] 未引入 JS/TS 执行能力（无 Deno/Node/QuickJS/sidecar）
- [x] 未读写 `~/.pi` / `.pi`
- [x] Session 存储仅 JSONL，未引入 SQLite 等其他后端
- [x] token 估算算法与常量未偏离钉死版 Pi
- [x] 非测试代码无 `unwrap()` / `expect()`（有不变式注释的除外）
- [x] 日志 / 错误消息中无 API key、token 等凭据
- [x] 范围排除项未被引入（无 server/evals/bun 对应物、无 pi-ai 包级 CLI、无 legacy 启动迁移）
- [x] grep/find 未引入外部 rg/fd 二进制下载机制
- [x] session 文件写入未加文件锁（锁仅限 auth/settings/trust）

### live smoke 矩阵（W7 检查结果）

检查方式：对 38 家 provider 的 key 环境变量逐一做存在性判断（`[ -n "${VAR+x}" ]` 风格），**只检查变量名是否存在，未读取/未打印任何变量值**，无 key 值写入任何文件或输出。

| Provider | key 环境变量 | 存在？ | 结果 |
|----------|--------------|--------|------|
| anthropic | `ANTHROPIC_API_KEY` / `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_OAUTH_TOKEN` | 否 | 豁免（无 key） |
| openai | `OPENAI_API_KEY` | 否 | 豁免 |
| google（generative-ai） | `GEMINI_API_KEY` / `GOOGLE_API_KEY` | 否 | 豁免 |
| google-vertex | `GOOGLE_APPLICATION_CREDENTIALS` / `GOOGLE_CLOUD_API_KEY` | 否 | 豁免 |
| mistral | `MISTRAL_API_KEY` | 否 | 豁免 |
| deepseek | `DEEPSEEK_API_KEY` | 否 | 豁免 |
| xai | `XAI_API_KEY` | 否 | 豁免 |
| zai / zai-coding-cn | `ZAI_API_KEY` / `ZAI_CODING_CN_API_KEY` | 否 | 豁免 |
| moonshotai / -cn | `MOONSHOT_API_KEY` / `MOONSHOTAI_API_KEY` | 否 | 豁免 |
| openrouter | `OPENROUTER_API_KEY` | 否 | 豁免 |
| groq | `GROQ_API_KEY` | 否 | 豁免 |
| cerebras | `CEREBRAS_API_KEY` | 否 | 豁免 |
| together | `TOGETHER_API_KEY` | 否 | 豁免 |
| fireworks | `FIREWORKS_API_KEY` | 否 | 豁免 |
| nvidia | `NVIDIA_API_KEY` | 否 | 豁免 |
| huggingface | `HF_TOKEN` | 否 | 豁免 |
| minimax / -cn | `MINIMAX_API_KEY` / `MINIMAX_CN_API_KEY` | 否 | 豁免 |
| qwen-token-plan / -cn | `QWEN_TOKEN_PLAN_API_KEY` / `QWEN_TOKEN_PLAN_CN_API_KEY` | 否 | 豁免 |
| xiaomi（四端点） | `XIAOMI_API_KEY` | 否 | 豁免 |
| azure-openai-responses | `AZURE_OPENAI_API_KEY` / `AZURE_OPENAI_RESOURCE_NAME` / `AZURE_OPENAI_API_VERSION` | 否 | 豁免 |
| amazon-bedrock | `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` / `AWS_PROFILE` | 否 | 豁免 |
| opencode / opencode-go | `OPENCODE_API_KEY` | 否 | 豁免 |
| ant-ling | `ANT_LING_API_KEY` | 否 | 豁免 |
| chutes | `CHUTES_API_KEY` | 否 | 豁免 |
| cloudflare 两家 | `CLOUDFLARE_API_TOKEN` / `CLOUDFLARE_ACCOUNT_ID` | 否 | 豁免 |
| github-copilot / openai-codex / kimi-coding / radius | OAuth 登录态（credential store） | 否（无本地凭据） | 豁免 |

`RPI_LIVE_TEST=1 cargo test -p rpi-ai --test live_smoke` 执行结果：**7 passed, 0 failed**（anthropic-messages / openai-completions / openai-responses / mistral-conversations / google-generative-ai / azure-openai-responses / bedrock-converse-stream 七个目标在无 key 环境下经 `gate()` 立即跳过通过，不访问网络）。

**遗留注记（小测试补漏，已随 W7 落地）**：`tests/live_smoke.rs` 原仅覆盖 T03 的 3 个适配器，T13 新增 7 适配器无 live 目标。W7 已按同一 gate 模式补充 mistral / google-generative-ai / azure-openai-responses / bedrock-converse-stream 四个 live 目标（env key 存在才跑；azure/bedrock 默认模型可经 `RPI_LIVE_*_MODEL` 覆盖）。openai-codex（OAuth 登录态、无标准 key env）、pi-messages（内部端点、无公开 key 约定）不加 live 目标，记录原因如上。本机无 key，补充目标同样豁免，不阻塞门禁。

### 需求 §5 逐条映射表

#### §5.1 KnownApi（10 个，全部实现）

| 需求条目 | 实现锚点 | 覆盖测试 |
|----------|----------|----------|
| openai-completions | `src/api/openai_completions.rs:2285`（stream）/ `:2330`（stream_simple）/ `:199`（detect_compat） | `tests/contract_adapters.rs:298` + 文件内单测 58 |
| openai-responses | `src/api/openai_responses.rs:596` / `:641`（+ `openai_responses_shared.rs`） | `tests/contract_adapters.rs:337` + 文件内单测 15 |
| azure-openai-responses | `src/api/azure_openai_responses.rs:614` / `:659`（+ `resolve_azure_config:222`、`normalize_azure_base_url:180`） | `tests/contract_azure_openai_responses.rs` 10 用例 |
| openai-codex-responses | `src/api/openai_codex_responses.rs:1480` / `:1525`（+ `api/codex_ws/` 子模块） | `tests/contract_openai_codex_responses.rs` 13 用例 |
| anthropic-messages | `src/api/anthropic_messages.rs:1582` / `:1655` | `tests/contract_adapters.rs:259` + 文件内单测 40 |
| google-generative-ai | `src/api/google_generative_ai.rs:852` / `:896`（+ `google_shared.rs`） | `tests/contract_google_generative_ai.rs` 33 用例 |
| google-vertex | `src/api/google_vertex.rs:1048` / `:1093`（+ `google_adc.rs`） | `tests/contract_google_vertex.rs` 23 用例 |
| bedrock-converse-stream | `src/api/bedrock_converse_stream.rs`（+ `api/bedrock/sigv4.rs`、`api/bedrock/event_stream.rs`） | `tests/contract_bedrock_converse_stream.rs` 31 用例 |
| mistral-conversations | `src/api/mistral_conversations.rs:1179` / `:1225` | `tests/contract_mistral_conversations.rs` 7 用例 |
| pi-messages | `src/api/pi_messages.rs:898` / `:934` | `tests/contract_pi_messages.rs` 9 用例 |

#### §5.2 协议适配器行为锚点

| 行为条目 | 实现锚点 | 覆盖测试 |
|----------|----------|----------|
| openai-completions：compat URL 自动检测矩阵（zai/together/moonshot/openrouter/cloudflare/nvidia/ant-ling/cerebras/xai/chutes/deepseek/opencode → 21 字段默认值；`model.compat` 部分覆盖回落） | `src/api/openai_completions.rs:199 detect_compat`；compat 全量数据烘焙于 `src/providers/data/*.json`（生成期） | `tests/compat_matrix.rs` 18 用例（逐 provider 命中 + catalog 烘焙 8 组 + 字段 roundtrip） |
| openai-completions：10 种 thinkingFormat / prompt_cache_key（sessionId 截 64）/ store:false / include_usage / usage 兜底 `choice.usage`（Moonshot）/ OpenRouter 路由偏好全字段 + cacheControlFormat + x-session-id / grammar tools（lark 优先、单调增量）/ zaiToolStream / Kimi deferred 序列化 / session affinity 三格式 | `src/api/openai_completions.rs`（thinking/缓存/序列化各处理函数）+ `api/openai_prompt_cache.rs` + `api/constrained_sampling.rs:48-174` | 文件内单测 58（thinkingFormat、grammar、usage 兜底、prompt cache 等）；`tests/compat_matrix.rs:257-403`（zaiToolStream/Kimi deferred/grammar/cache 烘焙）；`contract_adapters.rs:494` |
| openai-responses：encrypted reasoning 持久化 / TextSignatureV1 / tool_search 延迟工具 / prompt_cache_options / max_output_tokens 下限 16 / `call_id|item_id` 复合 id 与 `fc_<shortHash>` 重建 / compat 7 字段（azure 与 codex 复用） | `src/api/openai_responses.rs` + `openai_responses_shared.rs` | 文件内单测 15；`contract_adapters.rs:337/366`（交错 contentIndex）；`contract_azure_openai_responses.rs:316/384`（reasoning replay）；`compat_matrix.rs:307`（grammar 烘焙） |
| openai-codex-responses：WS 连接缓存 5min/55min、per-session SSE 永久回退、两类一次重试、缓存续传（基线前缀校验 + input delta）、zstd 压缩、`chatgpt-account-id` JWT、`originator:"pi"`、UA `pi (`、store:false+instructions+encrypted_content+verbosity、service tier 乘数 | `src/api/openai_codex_responses.rs:224-536`（URL/body/service tier）+ `api/codex_ws.rs`（§13 定稿状态机） | `tests/contract_openai_codex_responses.rs` 13 用例：`test_sse_contract_headers_and_zstd_body`、`test_websocket_cached_delta_and_reuse`、`test_websocket_connection_cache_ttls`、`test_websocket_idle_fallback_is_permanent`、`test_websocket_connection_limit_retry_once`、`test_websocket_previous_response_not_found_retry`、`test_websocket_one_shot_when_cache_retention_none` 等 |
| azure-openai-responses：AZURE_OPENAI_BASE_URL 归一化 / RESOURCE_NAME / API_VERSION 默认 v1 / DEPLOYMENT_NAME_MAP / options 新增 6 字段 / 3 host 后缀路径归一化 | `src/api/azure_openai_responses.rs:122-222`（parse_deployment_name_map / resolve_deployment_name / normalize_azure_base_url / build_default_base_url / resolve_azure_config） | `contract_azure_openai_responses.rs:595`（host 后缀归一化）、`:643`（config 默认值）、`:523`（deployment 走线）、`:569`（api-version/baseUrl options） |
| anthropic-messages：Claude Code 伪装（system 前缀/UA/x-app/beta 头）/ 17 条工具名 canonical 映射 / 自适应 vs 预算 thinking 双轨 / thinkingDisplay="summarized" / interleaved+fine-grained beta / cache_control ephemeral(1h) / x-session-affinity / message_start 起捕获 usage / stop 映射（refusal/pause_turn）/ tool call id 归一化 / tool_reference 排除规则 | `src/api/anthropic_messages.rs`（伪装、canonical 表、thinking 双轨、cache、usage 捕获） | 文件内单测 40；`compat_matrix.rs:403`（tool_references 默认矩阵）；`contract_adapters.rs:259` |
| google-generative-ai：模型族 thinking 分流（Gemini 3/Gemma 4 thinkingLevel 不可关 vs budget）/ budget 档位表 4 模型 × 4 档 / options.thinkingBudgets 优先 / usage 映射（input−cached、output+thoughts、cacheRead、cacheWrite=0、reasoning）/ thoughtSignature 保留 / VALIDATED / 无函数调用流式（单 delta）/ id 自增 | `src/api/google_generative_ai.rs:119-129`（模型族判定）+ `google_shared.rs:31-64`（thinkingLevel/thoughtSignature）+ usage 映射处理 | `contract_google_generative_ai.rs`：budget 表 5 用例（`test_budget_table_*`、`test_budget_dynamic_minus_one_*`、`test_custom_thinking_budgets_win`）、分流 2 用例（`test_thinking_level_split_gemini_3_and_gemma_4`、`test_thinking_disable_configs`）、signature 2 用例、工具 4 用例 |
| google-vertex：API key / ADC / project/location 解析 / `{location}` 占位符整体丢弃、不做模板替换 | `src/api/google_vertex.rs:125-189`（resolve_api_key / resolve_project / resolve_location / resolve_custom_base_url）+ `google_adc.rs`（ADC 子集） | `contract_google_vertex.rs`：resolve 系 6 用例（`:342`-`:556`，含 `test_resolve_custom_base_url_discards_location_placeholder`）、ADC 4 用例（service_account JWT-bearer / authorized_user refresh / token 端点失败 / 缺文件）、URL 规则 4 用例 |
| mistral-conversations：promptMode vs reasoningEffort 按模型二选一 / id 9 字符归一化（hash+碰撞重试）/ x-affinity+promptCacheKey / cached tokens 6 字段名变体 + 钳制 [0, promptTokens] | `src/api/mistral_conversations.rs:77`（MistralPromptMode）+ 归一化/cached 处理函数 | `contract_mistral_conversations.rs:300`（promptMode reasoning 流）、`:340`（reasoningEffort）、`:360`（tool call + id 归一化）、`:449`（cache retention）、`:468`（tool schema 严格序列化） |
| bedrock-converse-stream：SigV4 vs bearer / region 解析顺序 / header 白名单 / cachePoint 1h / interleaved thinking / EMPTY_TEXT_PLACEHOLDER 三处 / 自适应 thinking 族子串清单与 xhigh 子集 | `src/api/bedrock_converse_stream.rs:191-350`（config/region/header 白名单）+ `api/bedrock/sigv4.rs` + `event_stream.rs` | `contract_bedrock_converse_stream.rs`：SigV4 2 用例（`test_sigv4_signature_deterministic_over_the_wire`、`test_sigv4_sign_request_reference_vector`）、region 5 用例、header 2 用例、placeholder 3 用例（`test_convert_messages_empty_text_placeholder` 等）、族矩阵 2 用例（`test_supports_adaptive_thinking_family_matrix`、`test_supports_native_xhigh_effort_subset`）、cachePoint 1 用例 |
| pi-messages：单 POST /messages + SSE 回传 + done/error / contentSignature/redacted/rewrite（→ diagnostics）/ debug=1 | `src/api/pi_messages.rs:64-155`（options/rewrite impact/事件类型）+ `:898` | `contract_pi_messages.rs` 9 用例：`test_appends_debug_1_and_reports_response_headers_via_on_response`、`test_appends_rewrite_diagnostic_from_done_event`、`test_surfaces_backend_error_responses_with_diagnostics`、`test_propagates_server_sent_error_events` 等 |

#### §5.3 Providers（38 内置工厂 + 机制）

| 需求条目 | 实现锚点 | 覆盖测试 |
|----------|----------|----------|
| 38 个内置工厂（清单含区域拆分：zai/zai-coding-cn、minimax(+cn)、moonshotai(+cn)、xiaomi 四端点、qwen-token-plan(+cn)、cloudflare 两家、opencode/opencode-go、radius 等） | `src/providers.rs` spec 表（`id: "…"` × 38，`factory: Some` × 38 无占位）+ `src/providers/*.rs` 38 文件 | `tests/providers_group_a.rs`（15）+ `group_b`（11）+ `group_c`（8）+ `group_d`（12）共 46 用例；`providers.rs` 内单测（registry 与上游 all.ts 注册序对拍、catalog membership） |
| 混合 API provider 按 `model.api` 分发（github-copilot 3 API + filterModels 按 availableModelIds 过滤；opencode 4 API；opencode-go/xai/fireworks 各 2–3 API） | `src/models.rs:387 create_provider`（api map 分发）+ `providers/github_copilot.rs:76,124 filter_models` + `providers/radius.rs:144` | `providers_group_b.rs:99`（copilot 工厂与 auth）、`:139`（filter_models）、`:175`（copilot catalog claude 模型）；`github_copilot.rs` 内单测 `filter_models_only_narrows_for_oauth_with_valid_ids`；`oauth_copilot_radius.rs:12` 注记（登录后 account picker catalog 端到端） |
| createProvider 动态 catalog 机制（radius 纯动态；动态 overlay 与 baseline 按 id 合并；refreshModels 并发去重） | `src/models.rs:252 merge_models` + `:221 join_or_run`（inflight 去重）+ `providers/radius.rs`（refreshModels 装饰器）+ `crates/rpi/src/core/remote_catalog_provider.rs:208 refresh_models` | `providers_group_b.rs:460`（radius 工厂）；`remote_catalog_provider.rs` 单测 `inflight_refresh_dedups_concurrent_calls`、`offline_refresh_restores_stored_overlay_without_network`、`abort_during_fetch_rejects_and_skips_the_store_write` |
| cloudflare baseUrl 占位符物化（`{CLOUDFLARE_ACCOUNT_ID}` 等）；AI Gateway `cf-aig-authorization` 并删除 Authorization/x-api-key | `src/providers/cloudflare_stream.rs`（占位符物化）+ `src/auth/cloudflare_auth.rs:97-121` | `providers_group_b.rs:281`（workers-ai 需 account config）、`:325`（ai-gateway 需 account+gateway config 与 scoped env headers） |
| 内置模型目录为生成物（models.dev 数据源）：build.rs 生成 + 逐字节 vendored 37 JSON + manifest；generated.rs 惰性解析（get_builtin_* API 镜像 all.ts） | `build.rs:26` + `src/generated.rs` + `src/providers/data/*.json`（37 份 + `.manifest.json`，sha256 对拍）+ `scripts/refresh-model-catalog.sh` | `tests/model_catalog.rs` 4 用例：`test_vendored_files_match_manifest_sha256`、`test_vendored_set_matches_upstream_file_set`、`test_catalog_field_by_field_against_upstream`、`test_catalog_accessors_and_generated_at` |
| `rpi update --models` 远程 overlay（ETag/If-None-Match、4h 新鲜度、generatedAt 比对、15s 超时、endpoint 可配置 ADR-0002 §8）+ ModelsStore 持久化 + `refresh({allowNetwork:false})` 离线恢复 + force | `crates/rpi/src/core/remote_catalog_provider.rs` + `src/models.rs:515 refresh` + `src/models_store.rs:33-101` + `crates/rpi/src/cli/package_command.rs:52 parse_update_args` + `crates/rpi/src/core/model_runtime.rs`（接入） | `remote_catalog_provider.rs` 单测 14（`refresh_interval_constant_is_four_hours` 等）；`package_command.rs` 单测 7；`model_runtime.rs` 单测 8；`models_store.rs` 单测 |

#### §5.4 Auth

| 需求条目 | 实现锚点 | 覆盖测试 |
|----------|----------|----------|
| 解析顺序：显式 apiKey → credential store（命中即停）→ ambient；OAuth 过期 modify 锁内双重检查刷新；刷新失败抛错绝不回退 env | `src/auth/resolve.rs:105 resolve_provider_auth` + `src/auth/file_store.rs:430 FileCredentialStore` | `tests/auth_oauth_resolve.rs`；`providers_group_a.rs:295`（ANTHROPIC_AUTH_TOKEN 优先）、`:326`（OAuth token 优先于 API key env） |
| CredentialStore 契约 read/list/modify/delete；modify 唯一写路径 + 跨进程文件锁；list 只返回 {providerId,type}；判别式与 Pi auth.json 兼容（0600） | `src/auth/credential_store.rs:16`（trait/InMemory）+ `src/auth/file_store.rs`（文件实现） | `file_store.rs` 内单测 19 |
| env 变量表逐 provider 对齐（33 家 + 区域变体） | `src/auth/env_keys.rs:40-98`（`find_env_keys`/`get_env_api_key`） | `env_keys.rs` 内单测 4；providers_group 各工厂 auth 用例 |
| key 值解析 DSL：`!cmd` / `$VAR` / `${VAR}` / `$$` / `$!` 转义 | `src/auth/config_value.rs:240-433`（resolve_config_value / resolve_headers / 缓存） | `config_value.rs` 内单测 12 |
| OAuth 流程 7 个（anthropic PKCE / openai-codex PKCE+deviceauth 旁路 / github-copilot device code+policy-enable / openrouter 永久 key / kimi-coding / xai / radius） | `src/auth/oauth/{anthropic,openai_codex,github_copilot,openrouter,kimi_coding,xai,radius}.rs` + `load.rs` registry | `tests/oauth_codex_openrouter.rs`（5）+ `oauth_copilot_radius.rs`（5）+ `oauth_kimi_xai.rs`（6）；`auth/oauth/anthropic.rs` 内单测 10；`device_code.rs` 6 + `pkce.rs` 4 |
| device code 5 家（RFC 8628 参数 5s/slow_down/1s 下限/WSL 时钟漂移文案）+ codex deviceauth 变体 | `src/auth/oauth/device_code.rs`（框架）+ 各流程文件 | `device_code.rs` 内单测 6（轮询参数/时钟抽象）；各 oauth 测试文件（poll 抛错→Failed 文案等） |
| provider 自有 login：Bedrock（bearer-token/aws-profile/credential-chain）、Vertex（api-key/adc/service-account）、Cloudflare（多字段 prompt 存 credential.env） | `src/providers/amazon_bedrock.rs:29 login`、`src/providers/google_vertex.rs:30 login`、`src/auth/cloudflare_auth.rs:121 login` | `providers_group_a.rs:403`（vertex login 流程）、`:547`（bedrock login 流程）、`:598`（bedrock ambient 凭据） |
| `/login` `/logout` 订阅流；`checkAuth()` / `getAvailable()` | `crates/rpi/src/core/model_runtime.rs:1100 check_auth` / `:1109 get_available`（runtime 层已实现）；`/login` `/logout` slash 命令分发到 selector（`interactive_mode.rs:3517,3521` + `interactive/components/oauth_selector.rs`），但 selector 回调的 OAuth/API-key 登录流是 **stub**（`commands_selectors.rs:867,928` `TODO(T13)`，显示 "not available yet"） | `commands_selectors.rs` 内单测（login/logout selector，覆盖 stub 状态）；`model_runtime.rs` 单测 9 |
| AuthInteraction.prompt()（text/secret/select/manual_code，per-prompt signal 竞速取消）+ notify() | `src/auth/interaction.rs:124 AuthInteraction` | `interaction.rs` 单测（prompt 竞速取消）；各 OAuth 流程单测 |
| `options.env` 每请求环境覆盖 | `src/utils/provider_env.rs` + 各适配器（azure/vertex/bedrock/cloudflare） | 各契约测试 scoped env 用例（如 `contract_bedrock_converse_stream.rs` scoped_env、`contract_google_vertex.rs` env_with） |
| Rust 落地注记（fs2 锁语义、`!cmd` 仅 unix、快照保序、时钟抽象、测试缝） | 见偏离 D-008 / D-009（T04）+ D-033/D-034/D-035 | —（偏离已回写 `01-requirements.md` §5.4、`02-design.md` §3.5） |

#### §5.5 横切能力

| 需求条目 | 实现锚点 | 覆盖测试 |
|----------|----------|----------|
| stream 不抛出契约（失败编码为 error 事件 + stopReason；aborted 可继续） | `src/utils/event_stream.rs` + 各适配器 | `contract_adapters.rs:420`（`test_stream_does_not_throw_on_http_error`）；各适配器错误流用例 |
| thinking 统一级别 off~max；thinkingBudgets 四档；clampThinkingLevel（先上后下）；xhigh/max 预算路径降 high；默认预算表 | `src/models.rs:929 get_supported_thinking_levels` / `:954 clamp_thinking_level` + `api/simple_options.rs:46 clamp_reasoning` | `simple_options.rs` 单测 6；各适配器 thinking 用例 |
| maxTokens 钳制：contextWindow − 估算 − 4096，下限 1 | `src/models.rs:17 clamp_max_tokens_to_context` | 文件内单测；各适配器契约（max_tokens 走线断言） |
| Image input 占位文本 + image generation 子系统（ImagesModels/ImagesProvider，OpenRouter 非流式 modalities 永不 reject） | `src/images.rs` + `src/images/*` + `src/api/openrouter_images.rs:78 generate_images` | `tests/images.rs` 14 用例（请求形状/modalities/永不 reject/abort/on_payload/on_response/目录与集合） |
| transformMessages（cross-provider handoff）：孤儿 tool call 合成 / error+aborted 跳过 / redacted 跨模型丢弃 / thoughtSignature 删除 / thinking→文本 / id 归一化回填 | `src/utils/transform_messages.rs:127` | 文件内单测 9；`contract_adapters.rs:494`（跨 provider id 归一化）；六适配器 normalize 回调纯函数测试（D-036） |
| 两层重试：provider 层（x-should-retry、408/409/429/5xx、retry-after 优先级链、指数退避、maxRetryDelayMs 60s 超限立即失败）+ 外层 retryAssistantCall（两张 regex 表） | `src/utils/provider_retry.rs:185` + `src/utils/retry.rs:119,216` | `provider_retry.rs` 单测 11（优先级链/上限文案）；`retry.rs` 单测 6；`contract_adapters.rs:474`（retry 后成功） |
| Token/cost/cache 统计：calculateCost tiers 阶梯、cacheWrite1h 2× input、Codex service tier 乘数、totalTokens 合成 | `src/utils/cost.rs:12` | `cost.rs` 单测 3；`anthropic_messages.rs` 单测（cacheWrite1h）；`openai_codex_responses.rs:536 resolve_codex_service_tier` 用例 |
| overflow 三分支（pattern 表 + 排除表 / z.ai 静默 / Xiaomi 截断式） | `src/utils/overflow.rs:72` | `overflow.rs` 单测 5 |
| sessionId 选项（prompt_cache_key / session affinity / Codex WS 复用 / faux cache） | `api/openai_prompt_cache.rs` + 各适配器 + `rpi-test-support/src/faux.rs` | `openai_prompt_cache.rs` 单测；codex 契约（session-id 头）；faux 单测 2 |
| cacheRetention（none/short/long）：Anthropic ephemeral/1h、OpenAI 24h、Bedrock cachePoint、Mistral promptCacheKey；RPI_CACHE_RETENTION | 各适配器 + `openai_prompt_cache.rs` | `contract_bedrock_converse_stream.rs:1129`（cachePoint 1h）；`contract_mistral_conversations.rs:449`；anthropic 单测；compat_matrix 烘焙 |
| transport 偏好（sse/websocket/websocket-cached/auto；仅 codex 消费，其余静默忽略） | `src/api/openai_codex_responses.rs`（WS/SSE/auto） | codex 契约 5 用例（WS 基本流/续传/TTL/回退/一次性）；D-036 记录其余 provider 忽略 |
| 钩子：onPayload / onResponse / transformHeaders（大小写不敏感合并、null 删除） | `src/utils/headers.rs` + 各适配器 | `headers.rs` 单测；`contract_pi_messages.rs:424`（on_response）；`contract_bedrock_converse_stream.rs:900`（自定义头） |
| 工具参数校验：jsonschema 单路径 + 强转表 + 流式 partial JSON repair + sanitizeSurrogates 恒等 | `src/utils/validation.rs:305` + `src/utils/json_parse.rs:217,447` + `src/utils/sanitize_unicode.rs` | `validation.rs` 单测 9；`json_parse.rs` 单测 10（repair/partial）；D-006/D-007 回写 |
| constrainedSampling（json_schema strict prefer/require / grammar lark+regex） | `src/api/constrained_sampling.rs:150,174` | 文件内单测 5；`compat_matrix.rs:458`（grammar variants） |
| deferred tools：addedToolNames / splitDeferredTools / 各协议回退（Anthropic tool_reference / Kimi 序列化 / OpenAI tool_search） | `src/utils/deferred_tools.rs:24,88` + 各适配器 | `deferred_tools.rs` 单测 8；`compat_matrix.rs:403`（tool_references 矩阵）、`:285`（Kimi deferred 烘焙）；codex/pi 契约 |
| 诊断字面值三种 + redacted 布尔字段 + responseModel/responseId 回填 | `src/types.rs`（diagnostics 类型）+ `pi_messages.rs` + 适配器 | `contract_pi_messages.rs`（rewrite/response_failure 诊断用例）；`contract_adapters.rs`（responseModel 回填） |
| 代理：HTTP_PROXY/HTTPS_PROXY/no_proxy 解析（Codex fetch/WS 使用） | `crates/rpi/src/core/environment.rs:279-285`（常量 + 解析） | `environment.rs` 相关单测；上游 `node-http-proxy.test.ts` 意图覆盖 |
| Faux provider（脚本化队列/响应工厂/tokensPerSecond/usage 估算/cache 模拟/callCount） | `rpi-test-support/src/faux.rs`（T02，D-003 确定性化） | `faux.rs` 单测 2 + T02 对拍基建 |

#### §5.6 Provider 环境变量

| 需求条目 | 实现锚点 | 覆盖测试 |
|----------|----------|----------|
| 逐 provider env 变量对齐（33 家对照表 + 区域变体：QWEN_TOKEN_PLAN/_CN、ZAI/ZAI_CODING_CN、MINIMAX/_CN、Xiaomi 四端点、Moonshot 双 provider 共用 MOONSHOT_API_KEY） | `src/auth/env_keys.rs:40-98` 全表（`find_env_keys` 区域变体支持） | `env_keys.rs` 内单测 4；各工厂 auth 用例（`providers_group_c.rs:166` env key 解析等） |

#### 缺口清单（无锚点或锚点不全的条目）

| # | 条目 | 说明 | 建议 |
|---|------|------|------|
| 1 | `Models::get_available`（rpi-ai 层，models.rs 注释承诺 W5 落地） | 功能等价物在 `crates/rpi/src/core/model_runtime.rs:1109 get_available`；W7 审查补漏：`refresh_availability_inner` 已接入 per-provider `filter_models`（models.ts:394-408 对齐，含两个装饰器的 `filterModels` 转发，provider-composer.ts:492-494），runtime 路径由 `model_runtime.rs` 单测 `get_available_applies_copilot_filter_models` 覆盖；`models.rs:7-8/120` 过期注释已清理 | 已闭环（非缺口） |
| 2 | T13 新适配器 live smoke 目标 | W7 已补 mistral/google/azure/bedrock 4 个 live 目标（gate 模式与 T03 一致）；codex（OAuth 无标准 key env）与 pi-messages（内部端点）不加，原因已记录 | 已补（小测试补漏） |
| 3 | 需求 §5.6 引用的 `docs/providers.md` 对照表 | 上游 pi 仓库文档，未随 `external/pi` vendored 子集落地；rpi 侧权威对照表为 `env_keys.rs` 全表（对齐 env-api-keys.ts）。W7 审查补漏：需求 §5.4/§5.6 引用已加注说明 | 已闭环（引用已注明上游归属 + rpi 侧权威表） |
| 4 | openrouter `loginLabel` 未移植（D-032 遗留） | 上游 openrouter 工厂 `loginLabel` 字段无对应槽位 | 已登记于 D-032；随 T15 扩展接线时评估 |
| 5 | 交互模式 `/login` `/logout` OAuth/API-key 登录流 | selector 回调为 stub（`commands_selectors.rs:867,928` `TODO(T13)`）；runtime 层 `check_auth`/`get_available`/凭据存储均已实现，缺交互式登录接线（interactive-mode.ts:4925-4933 / :5063） | 功能缺口（已如实登记）；随 T15 或后续波次接线 |

### 上游测试意图移植清单（`packages/ai/test/*.test.ts`，114 文件逐文件标注）

分类：**P** = 已移植（rpi 测试有直接对应）；**C** = 意图覆盖（经契约/目录/单测）；**L** = live 不移植（须真实 API key/OAuth 登录态）；**N** = 不适用（TS 特有/类型级/进程内加载机制）。

统计：P=27、C=55、L=30、N=2（合计 114；分类见下各表）。

#### 适配器类（44 文件）

| 上游文件 | 分类 | 说明 / rpi 对应 |
|----------|------|-----------------|
| compat-env.test.ts | C | compat env 覆盖 → `detect_compat`（`openai_completions.rs:199`）+ `tests/compat_matrix.rs` |
| pi-messages.test.ts | P | → `tests/contract_pi_messages.rs`（9 用例，loopback mock server 同构） |
| mistral-reasoning-mode.test.ts | P | → `contract_mistral_conversations.rs:300,340`（promptMode/reasoningEffort） |
| mistral-tool-schema.test.ts | P | → `contract_mistral_conversations.rs:468`（tool schema 严格序列化） |
| google-shared-convert-tools.test.ts | P | → `contract_google_generative_ai.rs:803-922`（convert_tools 单测组） |
| google-shared-gemini3-unsigned-tool-call.test.ts | P | → `contract_google_generative_ai.rs:996` |
| google-thinking-signature.test.ts | P | → `contract_google_generative_ai.rs:363,786` |
| google-vertex-api-key-resolution.test.ts | P | → `contract_google_vertex.rs:342,353,367` |
| google-shared-image-tool-result-routing.test.ts | C | image tool result 路由 → `google_shared.rs` convert_messages 单测 |
| azure-openai-base-url.test.ts | P | → `contract_azure_openai_responses.rs:595,643` |
| azure-openai-responses-reasoning-replay.test.ts | P | → `contract_azure_openai_responses.rs:316,384` |
| bedrock-convert-messages.test.ts | P | → `contract_bedrock_converse_stream.rs:978,1063,1108` |
| bedrock-custom-headers.test.ts | P | → `contract_bedrock_converse_stream.rs:866-900` |
| bedrock-endpoint-resolution.test.ts | P | → `contract_bedrock_converse_stream.rs:719-840`（region 解析 5 用例） |
| openai-codex-stream.test.ts | P | → `tests/contract_openai_codex_responses.rs` 13 用例 |
| anthropic-auth-token.test.ts | P | → `providers_group_a.rs:295`（bearer 优先级） |
| anthropic-oauth.test.ts | C | OAuth 流程 mock 测试；意图由 `auth/oauth/anthropic.rs` 内单测 10 覆盖（T04） |
| anthropic-adaptive-thinking-models.test.ts | C | 自适应模型族 → anthropic_messages.rs 单测 + `compat_matrix.rs:403` |
| anthropic-cache-write-1h-cost.test.ts | C | → `utils/cost.rs` 单测 + anthropic 单测（cacheWrite1h 拆分） |
| anthropic-eager-tool-input-compat.test.ts | C | eager tool input compat → anthropic 单测 + catalog 烘焙 |
| anthropic-eager-tool-input-e2e.test.ts | L | `skipIf(!apiKey)`，须真实 key |
| anthropic-empty-thinking-signature-compat.test.ts | C | → anthropic 单测（空 signature 兼容） |
| anthropic-force-adaptive-thinking.test.ts | C | → anthropic 单测（强制自适应） |
| anthropic-long-cache-retention-e2e.test.ts | L | `skipIf(!apiKey)` |
| anthropic-opus-4-8-smoke.test.ts | L | `skipIf(!ANTHROPIC_API_KEY)` smoke |
| anthropic-sse-parsing.test.ts | C | → `api/sse.rs` 单测 8 + 各契约测试严格 SSE 解析 |
| anthropic-temperature-compat.test.ts | C | → anthropic 单测（temperature compat） |
| anthropic-thinking-disable.test.ts | L | 主为 `skipIf` E2E；disable 语义由 `contract_google_generative_ai.rs:738` 等契约覆盖 |
| anthropic-tool-name-normalization.test.ts | L | `skipIf(!oauthToken)`，OAuth 登录态 |
| openai-completions-cache-control-format.test.ts | C | → completions 单测 + `compat_matrix.rs:381` |
| openai-completions-empty-tools.test.ts | C | → completions 单测（空 tools 序列化） |
| openai-completions-prompt-cache.test.ts | C | → `api/openai_prompt_cache.rs` 单测 + completions 单测 |
| openai-completions-reasoning-details.test.ts | C | → completions 单测（reasoning details 持久化） |
| openai-completions-response-model.test.ts | C | → completions 单测（responseModel 回填） |
| openai-completions-retry.test.ts | C | → `utils/provider_retry.rs` 单测 11 + `contract_adapters.rs:474` |
| openai-completions-thinking-as-text.test.ts | C | → completions 单测（thinkingFormat string-thinking） |
| openai-completions-tool-choice.test.ts | C | → completions 单测（tool choice 序列化） |
| openai-completions-tool-result-images.test.ts | C | → completions 单测（图像 tool result） |
| openai-responses-compat.test.ts | C | → responses 单测 15 + compat 字段 |
| openai-responses-empty-tool-result.test.ts | C | → responses 单测 |
| openai-responses-foreign-toolcall-id.test.ts | C | → `contract_adapters.rs:494` + responses 单测（fc_ 重建） |
| openai-responses-message-id.test.ts | C | → responses 单测（message id） |
| openai-responses-partial-json-cleanup.test.ts | C | → `utils/json_parse.rs` 单测 10（partial JSON repair） |
| openai-responses-terminal-event.test.ts | C | → responses 单测（终态事件） |

#### Provider 工厂与目录类（28 文件）

| 上游文件 | 分类 | 说明 / rpi 对应 |
|----------|------|-----------------|
| image-model-data.test.ts | C | 图像目录数据 → `tests/images.rs` 目录校验用例（D-037 记录：意图以目录校验测试表达） |
| images-models.test.ts | C | `ImagesModels` 集合 → `tests/images.rs`（集合/refresh 去重用例） |
| openrouter-images.test.ts | C | → `tests/images.rs` 14 用例（请求形状/modalities/永不 reject）+ `api/openrouter_images.rs` |
| providers.test.ts | P | → `tests/providers_group_a-d.rs` 46 用例（builtinModels 注册/约束采样元数据/Kimi 定价/Anthropic 优先级/Bedrock/Cloudflare/Vertex login/envApiKeyAuth/createProvider 分发/faux） |
| env-api-keys.test.ts | P | → `src/auth/env_keys.rs` 内单测 4 |
| github-copilot-oauth.test.ts | P | → `tests/oauth_copilot_radius.rs`（5 用例） |
| kimi-coding-oauth.test.ts | P | → `tests/oauth_kimi_xai.rs`（kimi 流程用例） |
| openai-codex-oauth.test.ts | P | → `tests/oauth_codex_openrouter.rs`（codex 流程用例） |
| openrouter-oauth.test.ts | P | → `tests/oauth_codex_openrouter.rs`（openrouter 永久 key 用例） |
| radius-oauth.test.ts | P | → `tests/oauth_copilot_radius.rs`（radius 流程用例） |
| xai-oauth.test.ts | P | → `tests/oauth_kimi_xai.rs`（xai 流程用例） |
| oauth-auth.test.ts | C | → `auth/oauth/*` 各流程单测 + `resolve.rs` 单测 |
| oauth-device-code.test.ts | P | → `src/auth/oauth/device_code.rs` 内单测 6 |
| github-copilot-anthropic.test.ts | C | copilot anthropic 适配器行为 → `providers_group_b.rs:99,175` + 混合分发 |
| cloudflare-stream.test.ts | C | cloudflare 序列化 → `providers/cloudflare_stream.rs` 单测 |
| fireworks-models.test.ts | C | → catalog 校验（fireworks 目录字段） |
| qwen-token-plan-models.test.ts | C | → `providers_group_c.rs:299`（text-only 模型暴露） |
| together-models.test.ts | C | → `compat_matrix.rs:86` + catalog |
| xai-responses.test.ts | C | xai 走 responses 适配器 → xai 工厂 + responses 契约 |
| xiaomi-models.test.ts | C | → `providers_group_d.rs`（xiaomi 四端点目录） |
| openrouter-cache-control-models.test.ts | C | → `compat_matrix.rs:381`（openrouter cache 烘焙） |
| model-data-validation.test.ts | C | → `tests/model_catalog.rs`（字段级对拍） |
| model-catalog-types.test.ts | N | TS 类型级测试（`expectTypeOf`），Rust 无对应 |
| models-runtime.test.ts | C | → `crates/rpi/src/core/model_runtime.rs` 内单测 8 |
| bedrock-models.test.ts | C | bedrock 目录 → catalog 校验 + `providers_group_a.rs:523` |
| supports-xhigh.test.ts | C | xhigh 支持判定 → `simple_options.rs` 单测 + catalog |
| xhigh.test.ts | L | `skipIf(!OPENAI_API_KEY)` |
| zen.test.ts | L | `skipIf(!OPENCODE_API_KEY)` smoke |

#### 横切能力类（20 文件）

| 上游文件 | 分类 | 说明 / rpi 对应 |
|----------|------|-----------------|
| transform-messages-copilot-openai-to-anthropic.test.ts | P | → `utils/transform_messages.rs` 内单测 9 |
| deferred-tools.test.ts | C | → `utils/deferred_tools.rs` 单测 8 + 适配器层 tool_reference/Kimi/OpenAI 回退 |
| constrained-sampling.test.ts | C | → `api/constrained_sampling.rs` 单测 5 + `compat_matrix.rs:458` |
| error-body.test.ts | P | → `utils/error_body.rs` 内单测 4 |
| provider-error-body-passthrough.test.ts | C | → `error_body.rs` 单测 + 各适配器错误流（body 透传） |
| provider-error-body-regression.test.ts | C | 同上（逐 tier 回归） |
| provider-retry.test.ts | C | → `utils/provider_retry.rs` 单测 11 |
| retry.test.ts | C | → `utils/retry.rs` 单测 6 |
| overflow.test.ts | C | → `utils/overflow.rs` 单测 5（三分支） |
| validation.test.ts | C | → `utils/validation.rs` 单测 9（强转表/组合递归） |
| lax-message-content.test.ts | C | null content 归一化由 serde 边界容忍（`types.rs` null_default，D-036 记录） |
| unicode-surrogate.test.ts | L | live 部分 skipIf；sanitize 在 Rust 为恒等（D-007），意图无运行时步骤 |
| context-estimate.test.ts | C | → `utils/estimate.rs` + rpi-agent compaction 估算（T08 范围） |
| uuid.test.ts | P | → `src/utils/uuid.rs` 内单测 3（uuidv7 形状/时序/random） |
| reasoning-options.test.ts | C | → `api/simple_options.rs` 单测 6（clamp/预算） |
| max-thinking.test.ts | C | → `simple_options.rs` 单测（xhigh/max 降 high） |
| text.test.ts | C | → 适配器文本流契约（各 contract 正常流用例） |
| node-http-proxy.test.ts | C | → `crates/rpi/src/core/environment.rs` 代理解析 |
| faux-provider.test.ts | P | → `rpi-test-support/src/faux.rs`（T02 交付，D-003 确定性化） |
| lazy-module-load.test.ts | N | TS 动态 import 懒加载机制；Rust 静态链接无对应物（D-021/022/024/025/026/027/037 均记 lazy.ts 无对应物） |

#### live / E2E 类（22 文件，全部不移植）

| 上游文件 | 说明（不移植原因） |
|----------|---------------------|
| abort.test.ts | 逐 provider `skipIf(!key)`，须真实 key/OAuth |
| empty.test.ts | 逐 provider `skipIf(!key)` |
| stream.test.ts | "Generate E2E Tests" 全 live（7 provider 组 × 6 场景） |
| tokens.test.ts | 逐 provider `skipIf(!key)` |
| total-tokens.test.ts | 逐 provider `skipIf(!key)`（含 OAuth） |
| tool-call-without-result.test.ts | 逐 provider `skipIf(!key)` |
| image-tool-result.test.ts | 逐 provider `skipIf(!key)`（图像输入真实模型） |
| responseid.test.ts | 逐 provider `skipIf(!key)` |
| context-overflow.test.ts | `skipIf(!key)`；overflow 意图由 `overflow.rs` 单测覆盖 |
| cache-retention.test.ts | `skipIf(!ANTHROPIC_API_KEY)`；意图由 cache 契约覆盖 |
| cross-provider-handoff.test.ts | `skipIf(!hasAnyApiKey())`（D-036 专项记录） |
| openai-codex-cache-affinity-e2e.test.ts | `skipIf(!codexToken)` |
| openai-responses-cache-affinity-e2e.test.ts | `skipIf(!OPENAI_API_KEY)` |
| openai-responses-reasoning-replay-e2e.test.ts | `skipIf(!OPENAI_API_KEY\|\|!ANTHROPIC_API_KEY)` |
| openai-responses-tool-result-images.test.ts | `skipIf(!key)` |
| openrouter-cache-write-repro.test.ts | `skipIf(!OPENROUTER_API_KEY)` |
| images.test.ts | `skipIf(!OPENROUTER_API_KEY)` |
| google-thinking-disable.test.ts | `skipIf(!key)`（Anthropic/Google/Vertex 三组） |
| bedrock-thinking-payload.test.ts | `skipIf(!hasBedrockCredentials())` |
| interleaved-thinking.test.ts | `skipIf(!bedrock/anthropic 凭据)` |
| tool-call-id-normalization.test.ts | `skipIf(!copilot/openrouter/codex token)` |
| xiaomi-token-plan-ams-anthropic-empty-signature-smoke.test.ts | `skipIf(!apiKey)` |

（注：live 类 22 个文件中部分同时含 mock 化用例（如 abort/empty 的协议部分），其意图已由对应契约测试覆盖，逐条见适配器/横切表；其余 8 个 live 文件已在上表按类标注——anthropic-opus-4-8-smoke、anthropic-long-cache-retention-e2e、anthropic-eager-tool-input-e2e、anthropic-thinking-disable、anthropic-tool-name-normalization（适配器类）、xhigh（工厂类）、unicode-surrogate（横切类）、zen（工厂类）。）
