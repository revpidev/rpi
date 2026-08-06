# D-035：openai-codex / openrouter OAuth 流程 Rust 落地差异

- **状态**：已回写
- **关联任务**：T13（W5，OAuth）
- **级别**：实现细节偏离
- **发现日期**：2026-08-06

## 原文档约定

- 文档与章节：`docs/02-design.md` §3.4（Provider 层）、§3.5（Auth）；上游基准
  `packages/ai/src/auth/oauth/{openai-codex,openrouter}.ts` @ 0.82.1 (2efa728)
- 原文约定：§3.5 列出 7 个 OAuth 流程（openai-codex 为 PKCE +
  `id_token_add_organizations`/originator + deviceauth 旁路；openrouter 为
  PKCE 换永久 key、refresh no-op）；`oauth2` crate + `axum` 一次性 localhost
  回调页；`AuthInteraction`（prompt: text/secret/select/manual_code，
  notify: links/auth_url/device_code/progress）。

## 实际实现与偏离原因

两流程落 `pir-ai/src/auth/oauth/{openai_codex,openrouter}.rs`（工厂接线见
D-030 关闭记录与 D-032 第 2 项）。实现差异：

1. **测试缝**：上游 `vi.stubGlobal("fetch")` 打桩 + 绑定固定端口；Rust 侧为
   构造字段最小侵入缝（同 D-033 先例）——codex 用 `authority` URL 重写
   （`https://auth.openai.com/{path}` → `http://{authority}/{path}`，全部端点
   在同一上游主机上）+ `callback_port`（上游固定 1455）；openrouter 用
   `token_url`（`TOKEN_URL` 常量），回调服务器本就绑定临时端口，无需端口缝。
2. **回调服务器 axum 化**（coding-standards 附录 A，`oauth_page` HTML 复用
   `super::callback_page`，分支序与页面文案逐字保留）：
   - codex：固定 1455 `/auth/callback`；绑定失败 settle `None` 继续走
     manual 输入（上游 `server.once("error")` 返回 dummy server 分支）；上游
     500 catch-all 无对应物（handler 无可失败操作）；
   - openrouter：临时端口 + `/oauth/callback/{uuid}` 随机路径；绑定失败上抛
     （上游 listen 期 error reject）；`claimed || settled` → 409；key 交换在
     handler 内完成（`ExchangeFn` 闭包绑定 verifier/signal）；5 分钟登录超时
     与 30s 交换超时用 `tokio::time::timeout` 竞速代替
     `setTimeout`/`AbortController`（drop 请求 future 即中止）。
3. **JWT/随机数**：codex `atob` 宽松解码（base64url 与标准字母表、带/不带
   padding 四种组合依次尝试）；`randomBytes(16).toString("hex")` → ring RNG
   32 位 hex；openrouter `crypto.randomUUID()` → ring UUIDv4（D-033 先例）。
4. **错误通道**：deviceauth 轮询闭包内 fetch 错误经
   `DeviceCodePollResult::Failed` 同文案上浮（上游抛穿轮询框架，D-033 先例）；
   200 轮询响应体不可解析时以
   `Invalid OpenAI Codex device auth token response: null` 失败（上游为裸
   `SyntaxError`）。
5. **token JSON 严格化**（D-009 先例）：codex `readTokenResponse` 要求
   `access_token`/`refresh_token`/数值 `expires_in` 三字段齐备，缺字段报
   `... response missing fields: {...}`（上游 unchecked cast 产生 undefined
   条目）；`interval` 按 JS `Number` 语义解析（trim、空串 → 0、有限且 ≥ 0）。
6. **`expires` 语义**：codex `Date.now() + expires_in * 1000`（无 skew，与上游
   一致）；openrouter 永久 key 取 `Number.MAX_SAFE_INTEGER` 精确值
   9007199254740991；refresh no-op 原样返回凭据（`refresh` 的 `signal`
   参数接受并忽略）。
7. **浏览器流程竞速**：codex 的 manual 分支先 settle 时 `cancel_wait` 生效、
   `wait_for_code` 返回 `None` 后回退 manual 输入（上游 promise 图的两段
   `if (!code) await manualPromise` 检查收敛为 select 后的一次性回退）；
   openrouter `cancelWait` 在 `claimed` 时 no-op（交换进行中让登录以交换结果
   收尾）。codex 的 `State mismatch`（manual state 非空且不匹配）与
   `Missing authorization code` 文案逐字保留。
8. `parseAuthorizationInput` 按模块内联（上游两文件本就各自实现一份）：
   codex 版四分支含 `code#state`，openrouter 版仅 code（无 state 参数）。
9. 登录信号 → `CancellationToken`（`AuthInteraction::signal`）；codex
   `refresh` 无 signal/超时（上游同）；codex deviceauth 轮询与交换请求经
   `tokio::select!` 竞速信号（取消 → `Login cancelled`）。

## 影响面

无（纯内部）——不改变协议 / session 格式 / 扩展 API / TUI 行为契约；错误
文案与页面、轮询语义与上游逐字/逐分支对齐，仅实现载体（axum/reqwest/tokio）
与测试缝不同。

## 处置

- **回写位置**：`docs/02-design.md` §3.4（W4 波次注记）、§3.5（Rust 落地注记）
- **回写日期**：2026-08-06
- **ADR**：不需要
