# T27：扩展 API 面同步与 Wasm ABI 版本化

- **状态**：未开始
- **里程碑**：M3
- **依赖**：T21、T23
- **上游对照**：v0.84.0 扩展 Breaking 面（CHANGELOG）：`context.stored`/`publish()`、`refreshToken(credentials, signal)`、`isSubscription`、`ModelRegistry.refresh()` 签名、`setRuntimeApiKey()` async、`getApiKeyAndHeaders()` null 语义（#7030）；`04b15259b`+`b6fb91e5b`（ctx.scopedModels）、`1eb988cfe`（terminate）、`714978bf5`（registerMarkdownTransformer）、`9ab91fb93`（工具 prompt 贡献外露）；examples 迁移：`ab366ebe9`/`fed6009cc`/`393c34422`
- **需求章节**：v0.11 需求 §6（扩展 API 面 12 项汇总表）；设计 §6
- **预估**：0.45 人月

---

## 目标

扩展 SDK/宿主的 API 面与上游 v0.84 对齐。核心是 `refreshModels` context 重构——
这是 **Wasm ABI 级变更**，需版本化方案，处理不当会破坏已分发的 wasm 扩展包（设计 §8 风险 2）。

## 范围

### In

- **Wasm ABI 版本化方案先行评审**（设计 §6）：`refresh_models` 的 `stored` 快照 + `publish` 事务替代 `store` 读写——ABI 加版本化新 host function 集，旧函数保留一个周期并标记 deprecated；更新 `docs/extension-abi.md` 与 ADR-0007 缺口清单
- SDK 新增/变更：
  - `ctx.scoped_models` / `get_scoped_models`（只读快照，含 TUI 扩展上下文）
  - `tool_call` handler 返回 `terminate`（与 T22 的 agent loop 判定接线）
  - `register_markdown_transformer`（链式、宽度感知；context 含 `messageType`/`isStreaming`/`availableWidth`——TUI 侧接线在 T29）
  - `model_registry.complete/find/has_configured_auth` 统一入口（替代手动 auth + compat complete）
  - `set_runtime_api_key()` 改 async；参数改为 auth 取消选项
  - `get_api_key_and_headers()` 返回 `Option<String>` 值，**null 删除标记原样透传**（防占位凭证泄漏到 AI Gateway，#7030）
  - OAuth `refresh_token(credentials, token)` 取消令牌必选 + `is_subscription`
  - 工具 system prompt 贡献常量外露（bash/find/edit/read/write/grep/ls 各一）
- `ResourceLoader` 新方法（T24 落地）的扩展面暴露
- 内置 llama 扩展按新 `refreshModels` context 重写（上游 `extensions/llama/provider.ts` 已迁，pir 同步）
- 类型面同步：TUI 相关类型（`TuiMainScreen` 等）待 T28 后接线，本任务预留 SDK 类型占位

### Out

- Markdown transformer 的 TUI 渲染接线（T29）
- TUI 类型面（`TuiMode`/`ViewportTUI` 等）接线（T28/T32）

## 开发要点

- ABI 版本化设计在动工前以 ADR 或设计评审收口（旧函数保留策略、版本协商、向后兼容矩阵）
- `get_api_key_and_headers` 的 null 透传要有防泄漏回归：占位 OpenAI 凭证不得经 null 删除标记转发到 Gateway
- 88 条扩展 API 锚点清单（v0.1 `docs/parity-checklist.md`）同步更新本版本新增/变更项

## 进度跟踪

- [ ] 设计细化（含 ABI 版本化评审）
- [ ] 实现
- [ ] 自测
- [ ] 门禁验收
- [ ] 文档回写

## 自测清单

- [ ] ABI 版本化：旧 ABI v1 扩展包仍加载运行（deprecated 警告）；新函数集可用；版本协商矩阵全组合
- [ ] `refreshModels` 新 context：stored 只读 / publish 三态 / generation 检查（与 T21 核心一致）
- [ ] scopedModels 只读快照（Interactive/TUI 两种上下文）
- [ ] terminate 经扩展 handler 全链路（T22 判定的扩展入口）
- [ ] get_api_key_and_headers null 透传防泄漏回归（#7030 场景）
- [ ] llama 内置扩展迁移后 e2e 通过

## 门禁验收

通用门禁 G1–G7 全过（G5 重点：ABI 线格式；G6 重点：`docs/extension-abi.md` 与 ADR-0007 更新）。

任务特有标准：

- [ ] 需求 §6 汇总表 12 项逐条核对（每项编译期或运行时校验锚点）
- [ ] ABI 版本化评审结论附验收记录

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| （待登记） | | |

## 验收记录

（按 gates §3 模板填写）
