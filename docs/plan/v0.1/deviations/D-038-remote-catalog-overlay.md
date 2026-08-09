# D-038：远程模型目录 overlay 与 Models::refresh Rust 落地差异（remote-catalog-provider / ModelsStore 完整化 / radius refreshModels / rpi update --models）

- **状态**：已回写
- **关联任务**：T13（W6-C）
- **级别**：实现细节偏离
- **发现日期**：2026-08-06

## 原文档约定

- 文档与章节：`docs/02-design.md` §3.4（模型目录与 Provider 层）、§12（文件映射）；上游基准
  `packages/coding-agent/src/core/remote-catalog-provider.ts`、
  `packages/ai/src/models.ts`（`Models.refresh` / `createProvider.fetchModels`）、
  `packages/ai/src/providers/radius.ts`（`refreshModels`）、
  `packages/coding-agent/src/core/model-runtime.ts`（create/refresh 接线）、
  `packages/coding-agent/src/package-manager-cli.ts`（`update --models`）@ 0.82.1 (2efa728)
- 原文约定：`withRemoteCatalog` 装饰静态内置 provider；`Models.refresh` 并发刷新全部
  动态 provider（`allowNetwork ?? true`、`force`、signal、失败后 `allowNetwork:false`
  离线恢复、未配置跳过）；radius 刷新经 `{gateway}/v1/config`；`ModelRuntime.create`
  的 create 期网络刷新默认关、15s 超时；`update --models` 强制刷新并 15s 超时。

## 实际实现与偏离原因

1. **`createProvider.fetchModels` 钩子未移植**（models.ts:544, 596-616）：`CreateProviderOptions`
   增必填字段会打破 43 处构造点（41 个工厂文件，W6 三个代理并行在 `providers/` 施工），
   且当前无消费者（radius 与 remote catalog 都自带 `refreshModels` 实现）。
   通用 refresh 模式（store 恢复 → `allow_network` 检查 → fetch → 写 store）在
   `InflightRefresh` 去重槽（`models.rs` 公开类型）之上由 radius / remote catalog /
   `Models::refresh` 测试直接承载；`fetchModels` 随 provider-composer 扩展 refresh
   接线（T15）补齐。
2. **`Provider::refresh_models` 返回 `Option<BoxFuture>`**：Rust trait 无法表达
   「方法可选」；默认 `None` = 无动态 overlay，`Models::refresh` 先以不轮询的 probe
   调用判定（与上游 `refreshModels !== undefined` 过滤等价），非可刷新 provider 不
   做任何凭据解析（避免过期 OAuth 触发无谓刷新）。
3. **`withRemoteCatalog` 落 `crates/rpi/src/core/remote_catalog_provider.rs`**（按
   §2.3 文件映射；上游在 coding-agent）。需要 `rpi` 新增 `reqwest`/`httpdate`/`url`
   依赖（workspace 基线已有版本）。UA 按 ADR-0001 命名为 `rpi/{VERSION} ({os};
   rust; {arch})`——上游 `node/{version}` runtime 分量无对应物，以 `rust` 标记
   （`pi-user-agent.ts`）；其余头/URL/状态处理逐行对齐。
4. **`parseCatalog` 丢弃 serde 不可表达条目**：上游按 `"id" in entry` 过滤后以
   unchecked cast 保留残缺条目（字段为 undefined）；Rust 对不满足 `Model` 反序列化
   的条目直接丢弃（同 D-032 radius sanitize 先例——上游对 `Model` 消费者同样不可见）。
5. **store 错误映射**：`ProviderModelsStore` trait 返回 `AiError`（T03 定型），
   refresh 路径统一 `map_err` 为 `ModelsError("model_source", …)`（上游对非
   `Error` rejection 同码包装）。
6. **模型目录 `models-store.json` 损坏回退内存存储**：上游 `FileModelsStore` 惰性
   构造、读时 `JSON.parse` 抛错进 refresh 错误集；rpi `JsonFileModelsStore::load`
   急切加载，`ModelRuntime::create` 遇损坏文件 warn + 回退 `InMemoryModelsStore`
   （内部缓存文件，降级可接受）。
7. **`ModelRuntime` 未注册内置 provider**：T10 边界（38 工厂运行时注册属后续波次），
   故 `with_remote_catalog` 本波次无运行时消费者（模型目录 overlay 的注册波次按
   上游 model-runtime.ts:144-150 包装）；compose/overlay 路径已预留
   `refresh_models` 委托（`AuthOverridingProvider` + `RefreshDelegatingProvider`，
   对齐 provider-composer.ts:475-478）。
8. **`refresh()` 相关调用点语义**：`register_native_provider`/`register_provider`/
   `unregister_provider` 显式 `allow_network:false`（上游同）；`set_runtime_api_key`
   默认 `model_network_enabled`（= `RPI_OFFLINE` 未设）；`update --models` 显式
   `allow_network:true` 即使 `RPI_OFFLINE` 也强制拉取（上游 refreshModelCatalogs
   同）。
9. **测试基建**：上游 `vi.spyOn(globalThis, "fetch")` → rpi-ai 沿用既有 loopback
   axum mock（radius），rpi 侧手写 tokio TCP HTTP/1.1 脚本化响应服务器
   （`MockCatalogServer`，避免给 rpi 增 axum dev 依赖）。上游 `remote-catalog-provider
   .test.ts` 六用例全部移植（键值目录 + UA/TTL/force、generatedAt 新旧、ETag 复验 +
   304、501 丢 etag、429 保留 etag + 重验、501 无 overlay）；另补 4h 常量断言、
   离线恢复、inflight 去重、abort 竞态、目录三形状、URL 编码等。
10. **15s 超时**：位于 `model_runtime` create 期与 `update --models`（上游同层）；
    默认值收敛为 `DEFAULT_MODEL_REFRESH_TIMEOUT_MS = 15_000` 常量并被测试钉住，
    机制经注入短超时 + loopback 挂起端点测试（不等真实 15s）。

## 影响面

无（纯内部）——不改变协议 / session 格式 / 扩展 API / TUI 行为契约；`update --models`
为新增 CLI 目标（T14 其余目标仍占位）。

## 处置

- **回写位置**：`docs/02-design.md` §3.4（模型目录段落 + W4 波次注记 radius 项）、§12（映射表）；D-032 解决记录追加（第 5 项 refreshModels 闭环）
- **回写日期**：2026-08-06
- **ADR**：不需要
