# D-052：内置 provider 注册波次补齐（ModelRuntime::create 播种 + with_remote_catalog 运行时接线）

- **状态**：已回写
- **关联任务**：T13（W4/W6-C 遗留接线）
- **级别**：实现细节偏离
- **发现日期**：2026-08-09

## 原文档约定

- 上游实现：`packages/coding-agent/src/core/model-runtime.ts` @ 0.82.1 (2efa728)
  `create()`（:181-190）——
  `builtinProviderCatalog.builtinProviders()` 全量注册为 `defaultBuiltins`，除
  `radius` 外逐个包 `withRemoteCatalog(provider, options.catalogBaseUrl,
  getBuiltinModelDataGeneratedAt())`；models.json 经 provider-composer 与内置
  base 合成（同 id 覆盖、新 id 新增），用户 models.json 只写自定义/覆盖配置。
- 原文约定（`docs/02-design.md` §3.4/§12、T13 计划）：38 内置工厂与
  `withRemoteCatalog` 装饰器随 T13 落地，但「远程 catalog 的运行时消费随内置
  provider 注册波次接线（D-038），解析器已就位」——注册波次未排期。

## 实际实现与偏离原因

T13 交付了全部原料（`rpi-ai/src/providers.rs::builtin_providers()` 38 工厂、
`crates/rpi/src/core/remote_catalog_provider.rs::with_remote_catalog` 装饰器、
`model_catalog_endpoint` 解析器），但 `ModelRuntime::create` 从未播种：
`native_providers` 初始为空，唯一注册路径是扩展（llama）与测试。后果：
`/login` 选择器、模型解析、`rpi update --models` 只能看到 models.json +
扩展 provider，用户必须全量手写 models.json——与上游「只写自定义配置」的
行为契约不符（行为级影响，但属**缺口补全**而非差异，落地后与上游对齐）。

D-052 补齐注册波次：

1. `CreateModelRuntimeOptions` 新增 `catalog_base_url`（对齐上游
   `catalogBaseUrl?: string`，model-runtime.ts:77）。
2. `ModelRuntime::create` 在 `rebuild_providers()` 前把
   `builtin_providers()` 逐个插入 `native_providers`（上游注册序，观测序
   一致）；`radius` 原样透传，其余按 `model_catalog_endpoint(catalog_base_url)`
   结果包 `with_remote_catalog`；字面量 `off` / 关闭时不构造 overlay（零网络
   路径，ADR-0002 §8）。
3. services 层（`agent_session_services.rs`）读取 settings `modelCatalogUrl`
   传入；SDK 与 `rpi update --models` 走 env/默认解析（上游同——这两个路径
   本就不读 settings）。
4. `Models::refresh` 的「未配置 provider 跳过」门（models.ts:296-298）使
   启动与 `update --models` 不会对无凭据内置 provider 发起网络请求，与上游
   一致。

## 回写位置

- `docs/02-design.md` §产品 endpoint 配置化（远程 catalog 消费点已接线）
- `crates/rpi/src/core/model_runtime.rs` 模块注记（T10 subset / W6-C notes）
- `crates/rpi/src/core/remote_catalog_provider.rs` `model_catalog_endpoint` 注记

## 测试

- `create_seeds_builtin_providers`：38 内置全注册（anthropic 双 auth 方法、
  radius 在列、空 models.json 无 composition error）
- `catalog_off_registers_builtins_without_overlay`：`off` 不构造 overlay
  （`refresh_models` 缺席）
- `catalog_base_url_wraps_builtins_in_overlay`：配置 base URL 时 overlay 在列
  （radius 保持动态）
- `models_json_overlay_composes_over_builtin_base`：同 id models.json 与内置
  base 合成（baseUrl/apiKey/models 覆盖生效）
- 既有测试按新契约修正：`provider_order_is_insertion_order`（注册序断言
  过滤内置）、`get_available_applies_copilot_filter_models`（同步目录断言
  收敛到 provider 作用域）、`test_resolve_cli_model_no_models_is_error`
  （目录非空后命中上游「not found」分支，model-resolver.ts:603）
