# D-037：image generation 子系统 Rust 落地笔记

- **状态**：已回写
- **关联任务**：T13（W6-A）
- **级别**：实现细节偏离
- **发现日期**：2026-08-07

## 原文档约定

- 文档与章节：`docs/02-design.md` §3.3、§3.6、§12
- 原文约定：§3.6「图像子系统独立：`ImagesModels` / `ImagesProvider`（OpenRouter images，chat completions `modalities` 非流式，永不 reject）」；§12 模块映射表按上游文件逐一对齐

## 实际实现与偏离原因

移植范围：`packages/ai/src/{images,images-models,image-models,images-api-registry}.ts`、`image-models.generated.ts`、`providers/images/register-builtins.ts`、`providers/openrouter-images.ts`、`providers/all.ts` 的图像半区（`builtinImagesProviders`/`builtinImagesModels`）、`api/openrouter-images.ts`（+`.lazy.ts` 意图）、`types.ts` 图像类型。落地为 `crates/pir-ai/src/images.rs` + `images/` 目录（`generated.rs` / `image_models.rs` / `images_api_registry.rs` / `images_models.rs` / `providers.rs` + `providers/{register_builtins,openrouter_images}.rs`）、`crates/pir-ai/src/api/openrouter_images.rs`，类型入 `types.rs`。测试：`images_models.rs` / `openrouter_images.rs` 文件内单测 + `crates/pir-ai/tests/images.rs`（14 用例，loopback HTTP，覆盖 `openrouter-images.test.ts`、`images-models.test.ts` 与 `image-model-data.test.ts` 意图）。

逐项差异：

1. **文件组织**：上游这些文件在 `src/` 根与 `providers/` 下，Rust 按子系统聚合为 `src/images.rs` + `src/images/*`（沿 `providers.rs` + `providers/*` 先例）；`images.ts`（dispatch）并入模块根 `images.rs`。
2. **`ImagesApi` 开放联合 → `ImagesApiKind(String)` newtype**（D-002 对 `ApiKind` 同法）；`ImagesModel` 的 `TApi` 泛型折叠，`output` 为 `Vec<ImagesOutputModality>`（`"text" | "image"`），字段集为上游 `Omit<Model, api|provider|reasoning|contextWindow|maxTokens|compat>` 再补 `api`/`provider`/`output`。
3. **接口形状**：`ProviderImages`（上游接口，每 api 模块恰好导出 `generateImages`）→ Rust trait（方法返回 `Pin<Box<dyn Future + Send + 'static>>`）；registry 的 `ImagesFunction` → `Arc<dyn Fn(...) -> Pin<Box<dyn Future<Output = AssistantImages>>>`（调用方克隆入 future）。
4. **注册无 import 副作用**：`register-builtins.ts` 的模块加载副作用（上游 `images.ts` import 即注册）→ `images::generate_images` 首次 dispatch 前 `ensure_builtin_registered()` 惰性注册一次；若调用方已先行注册则不覆盖（与上游「import 时注册、用户后注册替换」净效果一致）。`createLazyLoadErrorImages` 的惰性 `import()` 失败路径为死代码（Rust 静态链接），不移植。
5. **dispatch 抛错 → `Result`**：`images.ts` `generateImages` 对未注册 api 抛 `No API provider registered for api: ...` → Rust 返回 `Err(String)`；`wrapGenerateImages` 的 api 不匹配检查（上游 throw）→ 以 `stopReason: "error"` 的 `AssistantImages` 表达（与 `createLazyLoadErrorImages` 同模式，api 函数层永不抛）。
6. **reqwest 直连替代 `openai` SDK**（D-005 惯例）：无 SDK UA/stainless 遥测头；SDK 层 `maxRetries: 0` 天然满足（重试由共享 `retry_provider_request` 驱动）；超时 → reqwest client timeout；取消 → `CancellationToken` 与 `request.send()` 的 `select!` 竞速；错误文案遵循 crate 的 openai-completions 组合（`"Request failed with status {status}: {body}"` via `format_provider_error`），而非 SDK 解析出的 `error.message`；上游 openrouter-images 无 `metadata.raw` 提取，故不提取。
7. **响应解析容忍度**恰与上游未加保护的读取对齐（`id`/`usage`/`choices` 可选；`content` 非字符串按上游 `typeof` 检查跳过；非 `data:` URL 与畸形 `data:` URL（`data:([^;]+);base64,(.+)` 语义）跳过）；整体畸形 body → `serde_json` 错误文本进 `errorMessage`。
8. **永不 reject 双层**：adapter `generate_images` 与 `ImagesModels::generate_images` 各自 catch-all；`errorMessage` 文本：模型/鉴权失败为 `ModelsError.message`，adapter 内为格式化错误。
9. **refresh 错误通道**：`ImagesRefreshFn` 返回 `Result<Vec<ImagesModel>, String>`；`ImagesModels::refresh(provider)` 恒包 `ModelsError("model_source", "Model refresh failed for {id}")`（`with_cause` 附原始消息）；上游 `instanceof ModelsError` 直通分支无可达场景（无图像 provider 从 fetch 抛类型化 ModelsError）。全量 `refresh()` 用 `join_all` 并发 best-effort（上游 `Promise.allSettled`，永不 reject）。
10. **`ImagesProvider::get_models` 的 try/catch 无 Rust 对应**（trait 方法为总函数；上游「坏实现 yield 无模型」的兜底不可表达，panic 会 unwind）。
11. **`image-models.generated.ts` 转写**：用 node 脚本结构等义转写为 `images/generated.rs`（40 个模型，`OnceLock` 惰性构建——const 上下文无法容纳 `String`/`Vec`）；上游 `scripts/generate-image-models.ts`（dev 期 OpenRouter 目录抓取脚本）不移植，`image-model-data.test.ts` 的解析器测试意图以目录校验测试（`catalog_lookup_returns_models` 等）表达。
12. **`ProviderImagesOptions = ImagesOptions & Record<string, unknown>`** 的额外键逃生舱丢弃（Rust 无对应物）。
13. **`.lazy.ts` 无对应物**（与其他适配器一致，D-021 等）：`openrouterImagesApi()` 静态返回 `Arc<dyn ProviderImages>`（`OpenRouterImages` 单元结构）。
14. **时间戳**：`Date.now()` → `SystemTime::now().as_millis() as i64`。
15. **on_payload 见 wire JSON**（camelCase OpenAI chat-completions 形状：`model`/`messages`/`stream`/`modalities`）。
16. **取消竞态**：预取消 signal → `stopReason: "aborted"` + `"Request aborted"`（`RetryError::Aborted` 文案）；发送期取消 → `select!` 分支；响应体读取完成后不再复查 signal（上游 SDK 同理，竞态保留）。
17. **`CreateModelsOptions` 共享结构**：`ImagesModels::new` 复用 `models.rs` 的 `CreateModelsOptions`（含 W6-C 新增的 `models_store` 字段），`models_store` 不被 ImagesModels 消费（上游 `createImagesModels` 同样不消费 modelsStore）。

## 影响面

- 协议：无（wire 形状与上游一致，含 `modalities`、`data:` URL 编码）
- session 格式：无
- 扩展 API：无
- TUI 行为：无
- 无（纯内部）✓

## 处置

- **回写位置**：`docs/02-design.md` §3.3（适配器清单）、§3.6（图像子系统展开）、§12（映射表）
- **回写日期**：2026-08-07
- **ADR**：不需要
