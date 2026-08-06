# D-031：xai 工厂 OAuth 槽 W4 阶段占位（`oauth: None`，W5 接线）

- **状态**：已关闭
- **关联任务**：T13
- **级别**：实现细节偏离
- **发现日期**：2026-08-07

## 原文档约定

- 文档与章节：`docs/02-design.md` §3.4（Provider 层）；上游基准 `packages/ai/src/providers/xai.ts` @ 0.82.1 (2efa728)
- 原文约定：上游 `xaiProvider()` 在工厂构造期同时注册 api-key auth 与
  `lazyOAuth({ name: "xAI (Grok/X subscription)", loginLabel: "Sign in with SuperGrok or X Premium", load: loadXaiOAuth })`。

## 实际实现与偏离原因

T13 波次划分把 6 个 OAuth 流程（含 xai）划给 W5（`docs/plan/v0.1/T13-providers-oauth.md`
W5 行），W4 阶段 2 只移植工厂本体。因此 `pir-ai/src/providers/xai.rs`
的 `xai_provider()` 以 `ProviderAuth { api_key: Some(...), oauth: None }`
落地：`ProviderAuth.oauth` 槽即 W5 接线点，文件头注释与
`tests/providers_group_d.rs::xai_oauth_is_w5_scope_placeholder` 固化该占位。
`lazyOAuth` 本身是为浏览器 bundle 分割存在的上游技巧（`auth/helpers.ts`
端口注记已述），pir 为原生二进制，W5 直接构造 OAuth 实现填入即可（同 D-029
kimi-coding 的处置模式）。

影响是阶段性的：W5 落地前 xai 仅支持 `XAI_API_KEY` api-key 登录，无
SuperGrok / X Premium 订阅 OAuth 入口。W5 完成后本偏离可关闭。

## 影响面

无（纯内部）——阶段性功能缺口，不改变协议 / session 格式 / 扩展 API / TUI 行为的最终契约。

## 关闭说明（2026-08-06）

T13 W5 已接线：`crates/pir-ai/src/auth/oauth/xai.rs` 移植上游 `xai.ts`
（RFC 8628 device code，refresh 不轮换时保留旧 token、缺 `expires_in` 默认
1h、5 分钟过期前移，`toAuth` 为 api key），`providers/xai.rs` 的
`ProviderAuth.oauth` 槽填入 `Some(xai_oauth())`；`load.ts` 对应物
`auth/oauth/load.rs` 注册 `"xai"` 表项；占位测试 `providers_group_d.rs`
改为断言真实流程（`xai_oauth_slot_wired`）。流程落地差异见 D-034。

## 处置

- **回写位置**：`docs/02-design.md` §3.4（Provider 层 W4 波次注记段）
- **回写日期**：2026-08-07
- **ADR**：不需要
