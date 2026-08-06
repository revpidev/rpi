# D-033：T13 W5 github-copilot / radius OAuth 流程 Rust 落地差异（测试缝、ring UUIDv4、poll 错误通道等）

> 复制本模板为 `D-NNN-<short-slug>.md` 并填写。登记说明见 [README.md](./README.md)。

- **状态**：已回写
- **关联任务**：T13（W5，OAuth）
- **级别**：实现细节偏离
- **发现日期**：2026-08-06

## 原文档约定

- 文档与章节：`docs/02-design.md` §3.5（Auth，OAuth flows 7）；上游基准
  `packages/ai/src/auth/oauth/{github-copilot,radius}.ts` @ 0.82.1 (2efa728)，
  对应测试 `test/{github-copilot-oauth,radius-oauth}.test.ts`。
- 原文约定：OAuth 流程逐上游文件移植；`oauth2` crate + axum 一次性回调页
  （§3.5 已决策）；上游测试经全局 `fetch` 打桩与 vitest fake timers 驱动。

## 实际实现与偏离原因

1. **github-copilot 用 URL 重写测试缝替代全局 fetch 打桩**：流程内所有
   `https://{host}/{path}` 在分发前经 `authority` 缝（`#[cfg(test)]` 构造器）
   重写为 `http://{authority}/{host}/{path}`——单环回 mock 即可扮演
   github.com / `api.*` / enterprise 主机，且录制路径仍保留目标主机，
   enterprise 域名断言不失真。域名→URL 推导本身由纯函数单测覆盖；
   `to_auth`（无副作用）不重写。与 anthropic.rs 的 `token_url`/
   `callback_port` 缝同一先例（D-009）。
2. **radius 回调服务为 `oauth/radius.rs` 内独立 axum 实现**，不复用
   `callback_page::OAuthCallbackServer`：上游 radius.ts 自带 node:http
   server，与 anthropic 的 `startCallbackServer` 分支顺序与文案不同
   （state 先验于 error/code、"OAuth state mismatch." / "Missing
   authorization code." / "Signed in to Radius. You may now close this
   page."、bind 失败 `once("error")` 分支 resolve 哑 server 使
   `waitForCode` 得 null）——逐分支对齐上游 radius 版；
   `REDIRECT_URI` 保持上游常量（`http://127.0.0.1:1456/oauth/callback`，
   对外 advertised 值），测试经 `callback_port` 缝绑临时端口并直接驱动回调。
3. **`crypto.randomUUID()` → ring 生成 UUIDv4**：仅作 OAuth `state`，唯一性
   语义等价，36 字符 v4 布局（version/variant 位）保持一致；不引入 uuid 依赖。
4. **poll 闭包抛错通道**：Rust 轮询框架 `DeviceCodePollResult` 无错误变体
   （D-009 时钟抽象同批设计），上游从 poll 闭包 rethrow 的错误（fetch 失败、
   未识别 OAuth error）以 `Failed{message}` 承载同一消息文本，login 拒绝
   语义等价（错误码 `oauth` 一致）。
5. **HTTP/JSON 细节**：`fetchJson` 的 `statusText` 取 reqwest canonical
   reason（D-021 先例）；网络错误文案为 reqwest 错误文本（D-009
   `formatErrorDetails` 近似先例）；JS `number` 字段（`expires_at` /
   `expires_in` / `interval`）按 `f64` 解析后窄化到 `i64`/`u64`；
   radius token JSON 严格 serde（anthropic.rs 先例），github-copilot 保留
   上游手工 typeof 校验与逐条文案；radius device 授权响应的缺字段检查保留
   JS falsy 语义（空串与 `expires_in: 0` 视为缺失）。
6. **`enableAllGitHubCopilotModels` = `futures::future::join_all`**（上游
   `Promise.all`）；per-model 失败按上游 `catch → false` 吞掉且结果被忽略；
   known-models 列表取 vendored 目录 `get_builtin_models("github-copilot")`
   ——与上游 `GITHUB_COPILOT_MODELS` 同源 JSON（D-028 管线）。
7. **credential extras 写入规则**：`enterpriseUrl` / `availableModelIds` /
   `scope` 仅在存在时写入 `OAuthCredential.extra`（上游对象字面量的
   `undefined` 值键在 JSON 序列化时丢弃，语义等价）；这些 extras 与 W4 已落
   地的 `filter_models`（`availableModelIds`）及 `radius_config`
   （`gatewayConfig`）读取方对接。

## 影响面

无（纯内部）——测试缝仅 `cfg(test)` 可达；对外凭据形状、请求线格式、
事件/错误文案与上游一致。

## 处置

- **回写位置**：`docs/02-design.md` §3.4（W5 注记段落）、§3.5（T13 W5 注记）
- **回写日期**：2026-08-06
- **ADR**：不需要
