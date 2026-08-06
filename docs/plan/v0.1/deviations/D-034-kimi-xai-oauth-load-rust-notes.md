# D-034：kimi-coding / xai OAuth 流程与 load.ts 对应物 Rust 落地差异

- **状态**：已回写
- **关联任务**：T13
- **级别**：实现细节偏离
- **发现日期**：2026-08-06

## 原文档约定

- 文档与章节：`docs/02-design.md` §3.4（Provider 层）、§3.5（Auth）；上游基准
  `packages/ai/src/auth/oauth/{kimi-coding,xai,load}.ts` @ 0.82.1 (2efa728)
- 原文约定：两流程均为 RFC 8628 device code（`waitBeforeFirstPoll`）；kimi-coding
  另有 `KIMI_CODE_OAUTH_HOST` / `KIMI_OAUTH_HOST` 环境覆盖与 30s 请求超时、
  refresh 指数退避重试（≤3 次）、`toAuth` 为 `Authorization: Bearer` 头；xai
  另有 refresh 不轮换时保留旧 refresh token、缺 `expires_in` 默认 1h、5 分钟
  过期前移、`toAuth` 为 api key；`load.ts` 以动态 import + 
  `registerBundledOAuthFlowLoaders` 实现按需/打包加载。

## 实际实现与偏离原因

两流程落 `pir-ai/src/auth/oauth/{kimi_coding,xai}.rs`（同 D-033 模式），
`load.ts` 对应物落 `pir-ai/src/auth/oauth/load.rs`：

- **测试缝**：上游 `vi.stubGlobal("fetch")` + `vi.stubEnv`；Rust 侧 kimi 为
  构造字段 `with_oauth_host`（env 覆盖链单独以 `EnvGuard` 单测钉住，避免进程级
  env 在并行测试间串扰），xai 为 `with_authority` URL 重写缝
  （`https://auth.x.ai/{path}` → `http://{authority}/auth.x.ai/{path}`，同
  github_copilot 先例）
- **取消语义**：`AbortSignal` → `CancellationToken`；请求期取消以
  `CANCEL_MESSAGE`（"Login cancelled"）落地（radius 同模式；上游 kimi 为原始
  `AbortError`、xai 显式抛 "Login cancelled"）；kimi refresh 循环顶部检查保留
  上游 "Kimi Code token refresh aborted" 文案
- **超时**：kimi 的 `AbortSignal.timeout(30s)` → client 级 reqwest timeout；
  xai 上游 `postForm` 未包 timeout，故 xai 无请求级超时
- **poll 闭包抛错** → `DeviceCodePollResult::Failed` 同文案（reqwest 错误文本
  近似，D-009 先例）
- **时间与数值**：`Date.now()` → `SystemTime` 毫秒；`interval` / `expires_in`
  以 f64（JS `number`）解析后窄化进 u64 事件字段与 i64 凭据字段；kimi 无过期
  前移、xai 5 分钟前移，逐字保留
- **`readJson` 语义**：JS `typeof json === "object"` 对数组亦为真——Rust 侧
  object|array 均保留原值用于错误回显（其余类型 → `null`）
- kimi `DeviceAuthorization.verification_uri` 仅做信任校验、不存储（上游登录只
  上报 complete URI，字段在流内无用）
- **`load.ts` 对应物**：pir 为静态链接，动态 import 无对应物，落
  `auth/oauth/load.rs` registry 函数表——`OAuthFlowLoader` = 零参构造器函数
  指针，provider id → loader 表（`load_oauth_flow` 查找）+ 具名
  `loadXxxOAuth`（上游函数名保留）；带参的 radius 不入表（用
  `createRadiusOAuth`）；openai-codex / openrouter 表项已于 W7 审查补漏时
  补齐（注册表 6 项，与上游 `OAuthFlowLoaders` 键集一致，radius 除外）
- refresh 指数退避为不可中断 sleep（上游 `setTimeout` 同样不可中断）

## 影响面

无（纯内部）——不改变协议 / session 格式 / 扩展 API / TUI 行为的最终契约。

## 处置

- **回写位置**：`docs/02-design.md` §3.4（Provider 层 W4/W5 波次注记段）、
  §3.5（Auth T13 W5 注记段）
- **回写日期**：2026-08-06
- **ADR**：不需要
