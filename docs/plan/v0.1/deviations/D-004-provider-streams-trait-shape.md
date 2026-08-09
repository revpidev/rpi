# D-004：ApiStream trait 形状 → ProviderStreams（同步返回事件流）

- **状态**：已回写
- **关联任务**：T03
- **级别**：实现细节偏离
- **发现日期**：2026-07-29

## 原文档约定

- 文档与章节：`docs/02-design.md` §3.3
- 原文约定：每个适配器实现 `ApiStream` trait（`async_trait`，
  `async fn stream(...) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send>>`）。

## 实际实现与偏离原因

实现为 `rpi_ai::models::ProviderStreams`：`fn stream(...) -> AssistantMessageEventStream`
与 `fn stream_simple(...) -> AssistantMessageEventStream` 两个同步方法，直接返回推送式
事件流句柄（对齐上游 `StreamFunction` 签名——上游 stream 同样不是 async fn，而是立即
返回 `AssistantMessageEventStream` 后在后台驱动 I/O）。async-trait + boxed stream
会改变背压与取消语义且与上游事件流 API 不对称，故按上游形状落地。

「错误编码为事件、不返回 Err」的契约不变。

## 影响面

无（纯内部 API；事件契约不变）。

## 处置

- **回写位置**：`docs/02-design.md` §3.3（trait 定义块）
- **回写日期**：2026-07-30
- **ADR**：不需要
