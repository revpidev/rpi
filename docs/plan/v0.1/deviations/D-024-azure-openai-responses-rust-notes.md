# D-024：azure-openai-responses 适配器 Rust 落地差异

> 复制本模板为 `D-NNN-<short-slug>.md` 并填写。登记说明见 [README.md](./README.md)。

- **状态**：已回写
- **关联任务**：T13（W2 适配器批）
- **级别**：实现细节偏离
- **发现日期**：2026-08-06

## 原文档约定

- 文档与章节：`docs/02-design.md` §3.3（API 适配层，reqwest 直连差异登记惯例，同 D-005/D-021/D-022/D-023）
- 原文约定：行为语义以 `external/pi` @ 2efa728 为准；`azure-openai-responses.ts` 委托 `openai` SDK 的 `AzureOpenAI` 客户端。

## 实际实现与偏离原因

`crates/pir-ai/src/api/azure_openai_responses.rs` 完整移植上游 `azure-openai-responses.ts`（`azure-openai-responses.lazy.ts` 为动态 import 包装，pir 静态链接无对应物）。行为锚点（deployment name map、3 个 Azure host 后缀路径归一化到 `/openai/v1`、API version 默认 `v1`、base URL 四级回退链、strict tools 默认支持、reasoning effort/summary、encrypted_content 重放）均逐条对齐，消息/工具转换与流处理复用 T03 的 `openai_responses_shared.rs`。落地差异：

1. **HTTP 为 reqwest 直连**，不经过 `openai` SDK 的 `AzureOpenAI` 客户端：SDK 的 `x-stainless-*` 遥测头、`User-Agent`、平台头不发送，无 SDK 默认超时（调用方设 `StreamOptions::timeout_ms`）。线格式已从钉死版 openai@6.26.0 SDK 源码核对：`POST {baseUrl}/responses?api-version={v}`，deployment 在请求体 `model` 字段（SDK 的 `/deployments/{model}` 路径重写集合 `_deployments_endpoints` 不含 `/responses`，故 URL 无 deployments 前缀），认证头为 `api-key`（非 `Authorization: Bearer`），`Accept: application/json`。
2. **`on_payload` 见 wire（snake_case）JSON**，而非 SDK 的 camelCase `ResponseCreateParamsStreaming` 对象（与 pir 其他适配器一致）。
3. **SSE 解析差异**：严格 `serde_json` 解析（SDK 用 `JSON.parse`），解析失败文案为 `Could not parse Azure OpenAI Responses SSE event: {error}; data={data}`（SDK 为 `SyntaxError` 文案）。
4. **错误格式化边界**：HTTP 状态/body 在调用点从响应提取（上游在 catch 块从 SDK 错误对象读取）；仅 HTTP 失败携带 status，故 `Azure OpenAI API error` 前缀适用范围与上游 `formatProviderError(normalizeProviderError(error), "Azure OpenAI API error")` 一致；无 status 的普通错误（如 `Invalid Azure OpenAI base URL`、缺 base URL）文案原样透传。
5. **`stream_simple` 缺 API key 报错进事件流**：上游 `streamSimple` 同步 throw；pir 与其他适配器一致地走 `immediate_error_stream`（D-004 形状）。
6. **scratch 字段无需擦除**：上游 catch 块删除 content block 上的 `partialJson`/`customInput` 暂存字段；pir 的流式暂存留在 `ResponsesStreamProcessor` 内部，从不进入 content block。

## 影响面

无（纯内部）：请求线格式（路径/query/头/体形状）经钉死版 SDK 源码核对一致；base URL 归一化、deployment map、API version 默认、reasoning replay、tool-call、错误流均有契约测试对拍（`crates/pir-ai/tests/contract_azure_openai_responses.rs`）。偏差点集中在 SDK 自有头与解析/错误文案，不进 session/RPC 线格式。

## 处置

- **回写位置**：`docs/02-design.md` §3.3（T13 批次适配器差异登记句追加 azure-openai-responses → D-024）
- **回写日期**：2026-08-06
- **ADR**：不需要
