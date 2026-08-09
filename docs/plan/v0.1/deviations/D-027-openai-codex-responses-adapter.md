# D-027：openai-codex-responses 适配器 Rust 落地差异（WS 状态机表达、tokio-tungstenite/reqwest 直连、zstd 恒压缩、JWT 多字母表等）

> 复制本模板为 `D-NNN-<short-slug>.md` 并填写。登记说明见 [README.md](./README.md)。

- **状态**：已回写
- **关联任务**：T13（W3）
- **级别**：实现细节偏离
- **发现日期**：2026-08-06

## 原文档约定

- 文档与章节：`docs/02-design.md` §3.3（API 适配层）、§13（Codex WS 状态机开放项）、§14（技术选型：tokio-tungstenite / zstd）；`docs/coding-standards.md` 附录 A（依赖基线）
- 原文约定：逐文件同构移植 `packages/ai/src/api/openai-codex-responses.ts`（+ `.lazy.ts`）；WS 状态机的 Rust 表达为开放项，落地后定稿回写。

## 实际实现与偏离原因

适配器落为 `crates/rpi-ai/src/api/openai_codex_responses.rs` + `crates/rpi-ai/src/api/codex_ws.rs`（连接状态机）+ `crates/rpi-ai/src/api/codex_ws/cache.rs`（连接缓存/TTL/debug stats/回退表）+ `crates/rpi-ai/src/session_resources.rs`（`session-resources.ts` 移植）。共享逻辑复用 `openai_responses_shared.rs`（`ResponsesStreamProcessor` 新增 `resolve_service_tier` 钩子承载 `resolveCodexServiceTier`）与 `openai_responses.rs` 的 service-tier 价格乘数。与上游的有意差异：

1. **WS 状态机的 Rust 表达**（§13 开放项定稿）：socket 在 busy 期间移出 cache entry（所有权随请求，上游是共享对象 + busy 标志）；5min 空闲 TTL 用 spawn 定时任务 + 代际计数（上游 `setTimeout`/`clearTimeout`）；`readyState` 可复用性检查用非阻塞 poll 探针替代，意外到达的数据帧存入 `entry.pending` 交付下次读取（上游读 readyState，不会消费帧）。连接缓存 TTL、per-session SSE 永久回退、两类一次重试、缓存续传 delta 语义与上游一致。
2. **传输层直连**：HTTP 用 reqwest（非 fetch）、WS 用 tokio-tungstenite（rustls + webpki-roots，与 reqwest 的 rustls-tls 一致，服务单文件 musl 目标）；无浏览器/bun 分支（proxy env、globalThis.WebSocket 探测等运行时探测不移植）。`combineAbortSignals` 不移植：头超时 deadline 与用户取消令牌直接 select。SSE body 阶段超时消息为 `Request was aborted`（上游为 Node `AbortError` 文案，上游无测试钉死该文案）。
3. **zstd 恒压缩**：Rust 侧 `zstd` crate 恒可用，SSE 请求体总是压缩并带 `content-encoding: zstd`（上游仅在浏览器/Vite 无 `node:zlib` 时回退未压缩）。
4. **JWT 解析**：payload 段依次尝试 standard/standard-no-pad/URL-safe/URL-safe-no-pad base64（上游 `atob` 仅标准字母表；真实 ChatGPT token 为 base64url——上游宽松性超集，失败文案一致）。
5. **User-Agent**：`pi ({os} {release}; {arch})` 中 `os/release/arch` 取自 `std::env::consts` + `libc::uname`（上游 `node:os`）。
6. **错误分类**：上游靠 `instanceof CodexApiError/CodexProtocolError/WebSocketCloseError` 分类重试/回退控制流；Rust 落为 `CodexError` 枚举（Api/Protocol/Close/Transport/RetryDelayExceeded/Aborted/Other + 内部 `RetryScheduled` 哨兵），谓词一一对应。
7. **session-resources**：注册表回调为不可失败的 `fn` 指针（上游可 throw 聚合为 AggregateError）；同步清理包装在无运行时句柄时退化为 drop socket（关闭 TCP，不发 close 帧）。
8. **SSE 解析**：复用共享 `SseDecoder`（`data:` 行去一个前导空格；上游 codex 自行 trim 每个 data 行）——与 D-005 家族同类。
9. **`stream_simple` 缺 API key 进事件流**（上游同步 throw），与其他 rpi 适配器一致；`on_payload` 见 snake_case wire JSON（同 D-021..D-026）。
10. **`openai-codex-responses.lazy.ts` 无对应物**：rpi-ai 静态链接，`lazyApi` 动态 import 不存在（同 D-021..D-026）。
11. **bug-compatible 保留**：上游 `connectWebSocket` 中 `delete wsHeaders["OpenAI-Beta"]` 因大小写不匹配是 no-op，`openai-beta: responses_websockets=2026-02-06` 实际随握手发送——Rust 侧保留该行为并注释。
12. **测试缝**：TTL 经 `set_codex_websocket_ttls_for_tests` 参数化（上游用 fake timers）；`codex-websocket-cached-probe.ts` 为手工探针工具，其意图由 debug stats 断言覆盖，不单独移植。
13. **新增依赖**：`tokio-tungstenite`（0.26，default-features=false + connect/handshake/rustls-tls-webpki-roots）、`zstd`（0.13）、`libc` 进入 rpi-ai（设计文档 §14 已预定前两者）。

## 影响面

无（纯内部）：线格式（请求体/头/WS 帧）与上游逐项对齐并有契约测试覆盖；事件序、usage/成本、stopReason 语义不变。

## 处置

- **回写位置**：`docs/02-design.md` §13（新增「Codex WebSocket 状态机（2026-08-06 定稿，T13-W3）」小节并移除开放项）；`docs/coding-standards.md` 附录 A（tokio-tungstenite / zstd / libc 行）
- **回写日期**：2026-08-06
- **ADR**：不需要
