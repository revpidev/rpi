# D-021：pi-messages 适配器 Rust 落地差异

> 复制本模板为 `D-NNN-<short-slug>.md` 并填写。登记说明见 [README.md](./README.md)。

- **状态**：已回写
- **关联任务**：T13（W1 适配器批 1）
- **级别**：实现细节偏离
- **发现日期**：2026-08-06

## 原文档约定

- 文档与章节：`docs/02-design.md` §3.3（Api 适配层，pi-messages 行为锚点：`/messages` 端点、rewrite 诊断、`debug=1`）
- 原文约定：适配器逐条移植上游行为锚点；HTTP 层差异已在 D-005 按 T03 范围登记，T13 适配器各自补充。

## 实际实现与偏离原因

`crates/pir-ai/src/api/pi_messages.rs`（移植 `packages/ai/src/api/pi-messages.ts`）与上游的可观测差异：

1. **`streamSimple` 的 `toolChoice`/`debug` 走私字段未移植**：上游从 `SimpleStreamOptions` 对象上直接读这两个字段（TS 结构类型允许）；pir 的 `SimpleStreamOptions` 结构体无此字段。二者经 `PiMessagesOptions` 在直接调用 `stream()` 时可用（与 openai-completions 的 D-005 同款处理）。
2. **SSE data JSON 解析失败文案**：上游 `JSON.parse` 抛 `SyntaxError` 文案进 error 事件；pir 为 `serde_json` 错误文案。解析失败仍终止流并转为 error 事件，语义一致。
3. **JS 稀疏数组 / 松散类型强转无语义对应**：`contentIndex` 越界时上游产生稀疏数组洞，pir 以空 text 块补齐；delta 落到类型不匹配的块时上游抛 `TypeError`（被外层 catch 转 error 事件），pir 丢弃该 delta。良构后端（contentIndex 严格顺序）下两者行为一致。
4. **`statusText` 来源**：reqwest 只暴露 `StatusCode::canonical_reason()`，不保留服务端原始 reason phrase；错误文案/诊断中的 statusText 用标准 reason。
5. **env 变量改名**：`PI_CACHE_RETENTION` → `PIR_CACHE_RETENTION`（requirements §5.5 既定改名）；pi-messages 的 `resolveCacheRetention` 语义保留——未设置时不给默认（backend 默认生效），与 anthropic 适配器默认 "short" 不同，故不复用其共享 helper。
6. **`truncateDiagnosticString` 计数单位**：pir 按 Unicode scalar 截断 8192，JS 按 UTF-16 code unit（BMP 内一致，与 D-014 同款处理）。
7. **`pi-messages.lazy.ts` 无对应物**：动态 import 代码分割意图在静态链接的 Rust 中不存在，`PiMessages` 常驻，无懒注册等价物（其余 T13 适配器同）。
8. **`Response.body` 空值分支不移植**：reqwest 始终暴露 body 流，上游 `"response has no body"` 分支不可达。

## 影响面

无（纯内部）。线格式（请求体 `{model, context, options}`、SSE 事件 JSON、camelCase 字段）、错误进流语义、rewrite 诊断形状均与上游一致，由 `crates/pir-ai/tests/contract_pi_messages.rs` 对拍覆盖（含 401 诊断、`debug=1`、thinking、tool-call、rewrite、无终止事件等锚点）。

## 处置

- **回写位置**：`docs/02-design.md` §3.3（补一句 T13 pi-messages 差异指引向本条）
- **回写日期**：2026-08-06
- **ADR**：不需要
