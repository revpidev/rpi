# D-022：mistral-conversations 适配器 Rust 落地差异

> 复制本模板为 `D-NNN-<short-slug>.md` 并填写。登记说明见 [README.md](./README.md)。

- **状态**：已回写
- **关联任务**：T13（W1 适配器批 1）
- **级别**：实现细节偏离
- **发现日期**：2026-08-06

## 原文档约定

- 文档与章节：`docs/02-design.md` §3.3（API 适配层，reqwest 直连差异登记惯例，同 D-005/D-021）
- 原文约定：行为语义以 `external/pi` @ 2efa728 为准；`mistral-conversations.ts` 委托 `@mistralai/mistralai` SDK。

## 实际实现与偏离原因

`crates/rpi-ai/src/api/mistral_conversations.rs` 完整移植上游 `mistral-conversations.ts`（`mistral-conversations.lazy.ts` 为动态 import 包装，rpi 静态链接无对应物）。行为锚点（promptMode vs reasoningEffort 分流、tool-call id 9 字符归一化、`x-affinity`/`promptCacheKey`、cached tokens 6 字段变体钳制）均逐条对齐。落地差异：

1. **HTTP 为 reqwest 直连**，不经过 `@mistralai/mistralai` SDK：SDK 的 `user-agent`（speakeasy 标识）与遥测头不发送，无 SDK 默认超时（调用方设 `StreamOptions::timeout_ms`）。请求 URL/方法/体（`POST {baseUrl}/v1/chat/completions`，snake_case 体、`Accept: text/event-stream`、`Authorization: Bearer`、`data: [DONE]` 终止）已从 SDK 源码核对一致（SDK 0.x `chatStream.ts`、各 component zod schema）。
2. **`on_payload` 见 wire（snake_case）JSON**，而非 SDK 的 camelCase 请求对象（与 rpi 其他适配器一致）。
3. **SSE 解析差异**：chunk 用严格 `serde_json` 反序列化（SDK 用 zod schema），解析失败文案为 `Could not parse Mistral SSE chunk: {error}; data={data}`（SDK 为 `SDKValidationError` 文案）；内容 chunk 以 JSON 值检视，未知 chunk 类型忽略而非报错（SDK discriminated union 产 `Unknown` 项，适配器同样跳过，语义等价）。
4. **错误格式化边界**：保留上游 `Mistral API error ({status}): {body 截断 4000}` 形状；body 为空时 fallback 为 `Request failed with status {status}`（SDK 会插入其 `SDKError` 自身 message，含 Content-Type/Body 回显）；传输层错误带 reqwest message（SDK 为 fetch `TypeError` 文案）。
5. **`x-affinity` 覆盖检查大小写不敏感**：上游在合并后的 plain record 上查精确小写键 `x-affinity`；rpi 经 `merge_headers_chain` 合并后按 ASCII 大小写不敏感判定（调用方显式提供的 `X-Affinity` 亦视为已提供）。
6. **`stripSymbolKeys` 不移植**：`serde_json::Value` 不可能携带 TypeBox symbol 键，Rust 侧为恒等（mistral-tool-schema.test.ts 的意图由 strict 序列化契约测试覆盖）。
7. **`partialArgs` 暂存于处理器侧 scratch map**：rpi `ToolCall` 无 `partialArgs` 字段；上游在 block 上挂暂存字段并在 finish 前删除，Rust 侧等价地不使其离开处理器。
8. **`retries: {strategy: "none"}` 的对应**：上游显式关闭 SDK 重试；rpi 走共享 `retry_provider_request`，`max_retries` 默认 0（等价），调用方显式设置时可启用重试（与其他 rpi 适配器一致）。

## 影响面

无（纯内部）：线格式（请求体 snake_case、SSE chunk）经 SDK schema 核对一致；事件序列、usage 记账、id 归一化均有契约测试对拍。偏差集中在错误文案与 SDK 自有头，不进 session/RPC 线格式。

## 处置

- **回写位置**：`docs/02-design.md` §3.3（适配器清单句追加 mistral-conversations → D-022 指引）
- **回写日期**：2026-08-06
- **ADR**：不需要
