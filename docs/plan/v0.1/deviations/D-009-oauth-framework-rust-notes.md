# D-009：OAuth 框架的 Rust 落地差异

- **状态**：已回写
- **关联任务**：T04
- **级别**：实现细节偏离
- **发现日期**：2026-07-31

## 原文档约定

- 文档与章节：`docs/02-design.md` §3.5；`docs/01-requirements.md` §5.4；`docs/coding-standards.md` 附录 A
- 原文约定：OAuth 流程用 `oauth2` crate；一次性 localhost 回调页用 `axum`；device 轮询 RFC 8628 参数对齐（默认 5s、slow_down +5s、下限 1s、WSL 时钟漂移文案）；Anthropic PKCE + 本地回调与 manual_code 竞速。

## 实际实现与偏离原因

按设计落地，以下实现细节与上游（`auth/oauth/*.ts`）存在差异：

1. **device code 时钟抽象**：`Date.now()` 改为单调时钟 `DeviceFlowClock` trait（生产 `TokioClock`），测试注入 FakeClock 替代 vitest 假定时器；轮询参数与三条文案逐字对齐。
2. **测试缝**：`TOKEN_URL` / `CALLBACK_PORT` 由常量变为构造器字段（默认值逐字不变）——Rust 测试并行，串行绑真端口不可行；上游靠 mock fetch + 串行测试。`REDIRECT_URI` 常量保持逐字。
3. **回调服务**：无 500 catch-all 分支（axum handler 无可失败操作）；anthropic 文案经 `CallbackPageCopy` 参数化传入（耦合层级与上游调用点一致）；host 覆盖变量为 `PIR_OAUTH_CALLBACK_HOST`（ADR-0001 §2 统一 `PIR_` 前缀，对应上游 `PI_OAUTH_CALLBACK_HOST`）。
4. **`formatErrorDetails` 近似**：reqwest 错误映射为 `TimeoutError|ConnectionError|Error: msg; cause=...`（沿 source 链），无 `errno`/`stack` 对应物。
5. **token 响应 JSON 严格化**：`access_token`/`refresh_token`/`expires_in` 缺失即解析失败（上游 `JSON.parse`+cast 容忍成 `undefined`）；多余 `scope` 字段照常忽略。
6. **竞速实现**：`tokio::select!` 替代 promise 图，settle 语义相同（任一落定即取消另一方）。
7. **杂项**：CLIENT_ID 硬编码 base64 解码后值（注释保留原始 base64 串），避免仅为 atob 引入 base64 正式依赖；PKCE 随机源为 oauth2 crate（rand thread_rng）替代 Web Crypto。

## 影响面

无（纯内部）。对外行为（轮询参数、错误文案、竞速语义、HTML 页面、请求/响应线格式）逐字对齐并有同名移植测试锚定。

## 处置

- **回写位置**：`docs/02-design.md` §3.5（Rust 落地注记）；`docs/01-requirements.md` §5.4（落地注记指针）
- **回写日期**：2026-07-31
- **ADR**：不需要
