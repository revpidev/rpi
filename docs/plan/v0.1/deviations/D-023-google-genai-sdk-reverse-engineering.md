# D-023：google-generative-ai 适配器 `@google/genai` SDK 反推与 reqwest 直连差异

> 复制本模板为 `D-NNN-<short-slug>.md` 并填写。登记说明见 [README.md](./README.md)。

- **状态**：已回写
- **关联任务**：T13（W1 适配器批 1）
- **级别**：实现细节偏离
- **发现日期**：2026-08-06

## 原文档约定

- 文档与章节：`docs/02-design.md` §3.3「来源空白（Google/Bedrock）」；`docs/01-requirements.md` §5.2 google-generative-ai 锚点
- 原文约定：上游 google-generative-ai 适配器委托 `@google/genai` SDK，基址模板、SSE 帧格式等传输层细节在 TS 源码中不可考；Rust 实现须从 SDK 行为与 Google 公开 API 规范反推，此类反推点登记为偏离

## 实际实现与偏离原因

`crates/pir-ai/src/api/google_generative_ai.rs` + `google_shared.rs` 以 reqwest 直连实现。反推依据为钉死在 `external/pi/node_modules/@google/genai`（随上游 0.82.1 安装）的 SDK 源码（`generateContentStreamInternal` / `generateContentParametersToMldev` / `tModel` / `processStreamResponse` / `throwErrorIfNotOK` / `NodeAuth.addKeyHeader`）。反推得出的线格式（对拍已按此 mock）：

- URL：`POST {model.baseUrl}/models/{modelId}:streamGenerateContent?alt=sse`（baseUrl 去尾斜杠；上游 createClient 设 `apiVersion: ""`，版本路径含在 baseUrl 内；id 已带 `models/`/`tunedModels/` 前缀时不重复加）
- 头：`x-goog-api-key: <key>`（SDK `addKeyHeader` 用户已设则跳过 → pir 放在 merge 链 base 位，用户头优先）
- 体：`{contents, systemInstruction?, tools?, toolConfig?, generationConfig?}`；`systemInstruction` 为 `{parts:[{text}], role:"user"}`（SDK `tContent(string)` 产物）；`generationConfig` 仅含 `temperature`/`maxOutputTokens`/`thinkingConfig`，全空时省略

与上游的可观测差异（均属 D-005 同类）：

1. SDK 遥测头（`user-agent` / `x-goog-api-client` = `gl-node/…`）不发送；SDK 无默认超时（pi 侧 `StreamOptions::timeout_ms` 由 reqwest 实现）。
2. SDK 默认无重试（pi 未传 `httpOptions.retryOptions`）→ `StreamOptions::max_retries` 对本适配器无效，与上游一致。
3. SSE 用共享 `SseDecoder`（SDK 为 `\n\n`/`\r\r`/`\r\n\r\n` 分隔符切分器，不支持 `event:` 字段——Google 流也不发）；事件 JSON 严格 `serde_json` 解析，失败措辞 `Could not parse Google SSE event: …`（SDK 为 `SyntaxError` 文案）。
4. 非 2xx 错误信息对齐 SDK `throwErrorIfNotOK`：`JSON.stringify(errorBody)`——JSON 体逐字、非 JSON 体包装为 `{"error":{message,code,status}}` 序列化；`normalizeProviderError` 对 `ApiError` 只取 status+message（body 已折进 message），最终 `errorMessage` 即该串，无 `status: body` 重组。
5. 流内错误探针：逐原始网络 chunk 尝试解析为 `{"error":…}`，`code ∈ [400,600)` 时报 `got status: {status}. {json}`（对齐 SDK `processStreamResponse` 的 chunk 级 `JSON.parse` 探针）；SSE 帧化的 error 事件与上游一样不做特殊处理（落入 finish reason 缺失路径）。
6. `usage.input = prompt − cached` 在 JS 中可为负（异常计数），pir `Usage` 为 u64，饱和为 0。
7. 保持点（非偏离）：`on_payload` 收到的仍是 SDK 层 params 形状 `{model, contents, config}`（上游同），线格式转换在 hook 之后执行，镜像 SDK 管线。

## 影响面

协议（仅对 Google API 的出站请求头集合与错误文案；线格式请求体/路径/语义与 SDK 一致）

## 处置

- **回写位置**：`docs/02-design.md` §3.3「来源空白（Google/Bedrock）」段末补充 google-generative-ai 反推结论与本文编号
- **回写日期**：2026-08-06
- **ADR**：不需要
