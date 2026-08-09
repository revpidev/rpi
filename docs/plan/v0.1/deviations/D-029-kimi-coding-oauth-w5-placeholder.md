# D-029：kimi-coding 工厂 OAuth 槽 W4 阶段占位（`oauth: None`，W5 接线）

- **状态**：已关闭
- **关联任务**：T13
- **级别**：实现细节偏离
- **发现日期**：2026-08-07

## 原文档约定

- 文档与章节：`docs/02-design.md` §3.4（Provider 层）；上游基准 `packages/ai/src/providers/kimi-coding.ts` @ 0.82.1 (2efa728)
- 原文约定：上游 `kimiCodingProvider()` 在工厂构造期同时注册 api-key auth 与
  `lazyOAuth({ name: "Kimi Code (subscription)", loginLabel: "Sign in with Kimi Code", load: loadKimiCodingOAuth })`。

## 实际实现与偏离原因

T13 波次划分把 6 个 OAuth 流程（含 kimi-coding）划给 W5（`docs/plan/v0.1/T13-providers-oauth.md`
W5 行），W4 阶段 2 只移植工厂本体。因此 `rpi-ai/src/providers/kimi_coding.rs`
的 `kimi_coding_provider()` 以 `ProviderAuth { api_key: Some(...), oauth: None }`
落地：`ProviderAuth.oauth` 槽即 W5 接线点，文件头注释与
`tests/providers_group_c.rs::kimi_coding_oauth_slot_awaits_w5` 固化该占位。
`lazyOAuth` 本身是为浏览器 bundle 分割存在的上游技巧（`auth/helpers.ts`
端口注记已述），rpi 为原生二进制，W5 直接构造 OAuth 实现填入即可。

影响是阶段性的：W5 落地前 kimi-coding 仅支持 `KIMI_API_KEY` api-key 登录，
无订阅 OAuth 入口。W5 完成后本偏离可关闭。

## 影响面

无（纯内部）——阶段性功能缺口，不改变协议 / session 格式 / 扩展 API / TUI 行为的最终契约。

## 关闭说明（2026-08-06）

T13 W5 已接线：`crates/rpi-ai/src/auth/oauth/kimi_coding.rs` 移植上游
`kimi-coding.ts`（RFC 8628 device code，`KIMI_CODE_OAUTH_HOST` /
`KIMI_OAUTH_HOST` 环境覆盖，refresh 指数退避重试 ≤3 次，`toAuth` 为 Bearer
头），`providers/kimi_coding.rs` 的 `ProviderAuth.oauth` 槽填入
`Some(kimi_coding_oauth())`；`load.ts` 对应物 `auth/oauth/load.rs` 注册
`"kimi-coding"` 表项；占位测试 `providers_group_c.rs` 改为断言真实流程
（`kimi_coding_oauth_slot_wired`）。流程落地差异见 D-034。

## 处置

- **回写位置**：`docs/02-design.md` §3.4（Provider 层工厂段落补注）
- **回写日期**：2026-08-07
- **ADR**：不需要
