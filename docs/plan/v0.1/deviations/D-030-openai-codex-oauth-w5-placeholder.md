# D-030：openai-codex 工厂 auth W4 阶段占位（空 `ProviderAuth`，W5 接线）

- **状态**：已关闭
- **关联任务**：T13
- **级别**：实现细节偏离
- **发现日期**：2026-08-07

## 原文档约定

- 文档与章节：`docs/02-design.md` §3.4（Provider 层）；上游基准 `packages/ai/src/providers/openai-codex.ts` @ 0.82.1 (2efa728)
- 原文约定：上游 `openaiCodexProvider()` 在工厂构造期注册**唯一** auth 方式
  `lazyOAuth({ name: "OpenAI (ChatGPT Plus/Pro)", load: loadOpenAICodexOAuth })`
  （无 api-key 通道）。

## 实际实现与偏离原因

T13 波次划分把 OAuth 流程（含 openai-codex）划给 W5（`docs/plan/v0.1/T13-providers-oauth.md`
W5 行），W4 阶段 2 只移植工厂本体。与 D-029（kimi-coding 有 api-key 通道、`oauth: None`
占位）不同，openai-codex 上游无 api-key 通道，因此
`pir-ai/src/providers/openai_codex.rs` 的 `openai_codex_provider()` 以
`ProviderAuth::default()`（api_key/oauth 皆 `None`）落地：工厂、目录模型、
`openai-codex-responses` 适配器接线均已就位，但 auth 解析恒为未配置，
W5 填入 OAuth 实现即闭环。`lazyOAuth` 本身是为浏览器 bundle 分割存在的上游技巧
（`auth/helpers.rs` 端口注记已述），pir 为原生二进制，W5 直接构造填入即可。
文件头注释与 `tests/providers_group_a.rs::test_openai_codex_factory_config`
固化该占位。

影响是阶段性的：W5 落地前 openai-codex 无法通过任何 auth 解析（`Models::get_auth`
返回未配置）。W5 完成后本偏离可关闭。

## W5 关闭记录（2026-08-06）

`auth/oauth/openai_codex.rs`（PKCE + `id_token_add_organizations`/originator +
deviceauth 旁路 + refresh，上游 `packages/ai/src/auth/oauth/openai-codex.ts`
@ 0.82.1 2efa728）已落地，`openai_codex_provider()` 的 `ProviderAuth.oauth` 槽
填入 `openai_codex_oauth()`（display name "OpenAI (ChatGPT Plus/Pro)"，无
api-key 通道的约定保持）；`tests/providers_group_a.rs::test_openai_codex_factory_config`
断言已更新。本偏离闭环。

## 影响面

无（纯内部）——阶段性功能缺口，不改变协议 / session 格式 / 扩展 API / TUI 行为的最终契约。

## 处置

- **回写位置**：`docs/02-design.md` §3.4（Provider 层 W4 波次注记）
- **回写日期**：2026-08-07
- **ADR**：不需要
