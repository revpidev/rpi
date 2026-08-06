# D-025：google-vertex 适配器 SDK 反推、ADC 子集自实现与 reqwest 直连差异

> 复制本模板为 `D-NNN-<short-slug>.md` 并填写。登记说明见 [README.md](./README.md)。

- **状态**：已回写
- **关联任务**：T13（W2 适配器批 2）
- **级别**：实现细节偏离
- **发现日期**：2026-08-06

## 原文档约定

- 文档与章节：`docs/02-design.md` §3.3「来源空白（Google/Bedrock）」；`docs/01-requirements.md` §5.2 google-vertex 锚点（API key/ADC 鉴权、project/location 解析、baseUrl `{location}` 占位符整体丢弃）
- 原文约定：上游 google-vertex 适配器委托 `@google/genai` SDK（vertexai 模式），传输层细节须从 SDK 行为与 Google 公开规范反推，反推点登记为偏离

## 实际实现与偏离原因

`crates/pir-ai/src/api/google_vertex.rs` + `google_adc.rs` 以 reqwest 直连实现，复用 W1 的 `google_shared.rs`（D-023 口径延续）。反推依据：钉死的 `@google/genai` 1.52.0（`ApiClient` 构造器 / `getRequestUrlInternal` / `shouldPrependVertexProjectPath` / `tModel` / `generateContentParametersToVertex` / `NodeAuth`）与 `google-auth-library` 10.6.2（`GoogleAuth` ADC 链 / `gtoken` JWT-bearer / `oauth2client` refresh grant / `Compute` metadata）。

反推得出的 vertex 线格式（对拍已按此 mock）：

- URL：`POST {base}[/{apiVersion}][/projects/{project}/locations/{location}]/{tModel}:streamGenerateContent?alt=sse`；`tModel` 为 vertex 规则（裸 id → `publishers/google/models/{id}`，`owner/model` → `publishers/{owner}/models/{model}`，已带 `publishers/`/`projects/`/`models/` 前缀原样）。base 选择：自定义 baseUrl（`resolve_custom_base_url` 接受时，去尾斜杠、含 `vN`/`vNbetaM` 路径段则不追加 apiVersion、`ResourceScope.COLLECTION` 不 prepend project/location）> API key 模式或 location=global → `https://aiplatform.googleapis.com` > 多区域 `us`/`eu` → `https://aiplatform.{location}.rep.googleapis.com` > 其余 → `https://{location}-aiplatform.googleapis.com`；apiVersion 为 pi 钉死的 `v1`。
- 鉴权头：API key 模式 `x-goog-api-key`；ADC 模式 `authorization: Bearer <token>`；均置于 merge 链 base 位，用户头优先（SDK skip-if-present 语义）。
- 体：与 D-023 同形（`{contents, systemInstruction?, tools?, toolConfig?, generationConfig?}`；`generationConfig` 全空省略；`systemInstruction` → `{parts:[{text}], role:"user"}`）。vertex 侧 `partToVertex`/`toolToVertex`/`functionDeclarationToVertex` 对 pi 用到的字段逐字透传（已核对）。

与上游的可观测差异（均属 D-005/D-023 同类）：

1. SDK 遥测头（`user-agent`/`x-goog-api-client`）不发；SDK 默认无重试 → `max_retries` 无效，与上游一致。
2. SSE 用共享 `SseDecoder` + 严格 `serde_json`（SDK 为分隔符切分器 + `JSON.parse`），解析失败措辞不同；非 2xx 错误信息对齐 `throwErrorIfNotOK`（JSON 体逐字）；chunk 级流内错误探针（`got status: {status}. {json}`）同 D-023。
3. `on_payload` 收到的仍是 SDK 层 params 形状 `{model, contents, config}`（上游同）。
4. **ADC 子集自实现**（`google_adc.rs`，无对应上游 pi 文件——上游委托 google-auth-library）：解析链 `GOOGLE_APPLICATION_CREDENTIALS` → well-known gcloud 文件（`~/.config/gcloud/application_default_credentials.json`）→ GCE metadata server → `NO_ADC_FOUND` 原文文案。支持 `service_account`（RS256 JWT-bearer，payload `{iss, scope, aud, exp: iat+3600, iat}`，scope 固定 cloud-platform，token 端点为库内固定常量 `https://oauth2.googleapis.com/token`——v10 起不再读凭据文件 `token_uri`）与 `authorized_user`（refresh grant）。
5. **功能缺口**：`external_account` / `external_account_authorized_user` / `impersonated_service_account` 凭据类型显式报错（workload identity 联邦链未移植）；`fromStreamAsync` 的 JSON 解析失败回退 PEM/p12 的 GAPIC 老路径未移植。
6. metadata token 请求兼作 GCE 可用性探测（3s 超时，对齐 `gcp-metadata.isAvailable`；上游为独立 probe + 无超时 token 请求两跳）。
7. token 端点失败文案：body 含 `error` 字段时对齐 gtoken `{error}: {error_description}`，否则 HTTP 状态概述（gaxios 错误面近似，同 D-009「错误明细近似」口径）。
8. 测试缝（同 D-009 口径）：`AdcEndpoints`（`#[doc(hidden)]`，覆盖 token/metadata/well-known 路径）与 `google_vertex::resolve_request_url`（`#[doc(hidden)]`，端点选择矩阵断言）；上游对应值为库内常量。
9. token 按次获取：pi 每次 `stream()` 新建 `GoogleGenAI`/`GoogleAuth`，凭据缓存不跨调用，pir 同样每次流调用解析一次 token。
10. 新增 workspace 依赖 `ring`（RS256 签名）与 `base64`（JWT 编码；此前仅 pir crate 使用）进 pir-ai 依赖链——两者均已在 Cargo.lock（ring 由 rustls 传递引入），不改变发布依赖闭包。
11. vertex 变体的 thinking 配置表与 Gemini API 适配器不同已逐行核对：无 Gemma 4 分支、`getGoogleBudget` 无 `2.5-flash-lite` 行（2.5-flash 高阶 24576）；`stream_simple` 不做 API key 预检（缺 key 即合法 ADC 路径，与上游一致，区别于 google-generative-ai 的 eager error）。
12. `google-vertex.lazy.ts` 仅做动态 import 延迟加载，Rust 无对应物（同 D-021/D-022 口径）。

## 影响面

协议（仅对 Google Vertex API 的出站请求头集合、token 端点交互与错误文案；线格式请求体/路径/语义与 SDK 一致）+ 依赖基线（ring/base64 进 pir-ai）

## 处置

- **回写位置**：`docs/02-design.md` §3.3「来源空白（Google/Bedrock）」段末补充 google-vertex 反推结论与本文编号；`docs/coding-standards.md` 附录 A 依赖基线补 ring/base64 用途行
- **回写日期**：2026-08-06
- **ADR**：不需要
