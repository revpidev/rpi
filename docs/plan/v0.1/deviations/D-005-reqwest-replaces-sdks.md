# D-005：适配器 HTTP 层 reqwest 直连替代官方 SDK 的可观测差异

- **状态**：已回写
- **关联任务**：T03
- **级别**：实现细节偏离
- **发现日期**：2026-07-29

## 原文档约定

- 文档与章节：`docs/02-design.md` §3.3、§14（HTTP 选型 reqwest/rustls）
- 原文约定：HTTP 用 reqwest 已决策，但 SDK 缺失带来的逐项可观测后果未记录。

## 实际实现与偏离原因

三适配器（anthropic-messages / openai-completions / openai-responses）均以 reqwest
直连 HTTP + 自写 `SseDecoder`，不经过 `@anthropic-ai/sdk` / `openai` SDK。逐项后果：

1. 不发送 SDK 遥测/平台头（`x-stainless-*`、SDK 默认 `User-Agent`）；
2. 无 SDK 默认超时（调用方经 `StreamOptions::timeout_ms` 显式设置）；
3. SSE 帧 JSON 用严格 `serde_json` 解析（SDK 用 `JSON.parse`），解析失败的消息
   文本为 rpi 自定义（如 `Could not parse OpenAI Responses SSE event: {error};
   data={data}`），而非 JS `SyntaxError` 文案；
4. OpenRouter `error.metadata.raw` 细节从 HTTP 错误响应 body 解析（上游从 SDK
   error 对象读取）；
5. openai-responses 的 `OpenAI API error` 错误前缀只可能出现在 HTTP 错误上
   （上游 `formatProviderError(normalizeProviderError(error), prefix)` 对无 status
   的错误同样不加前缀，语义等价）；
6. 流式 scratch（`partialJson`/`customInput`）只存于处理器，不落在内容块上，
   上游 catch 中的 scrub 步骤无对应物。

以上差异均不进 session 线格式契约（错误文案本就不参与对拍锚点，见
`fixtures/README.md` §2 归一化粒度）。

## 影响面

协议（仅请求头集合与错误文案；请求/响应 body 线格式不变）。

## 处置

- **回写位置**：`docs/02-design.md` §3.3（适配层注记）
- **回写日期**：2026-07-30
- **ADR**：不需要
