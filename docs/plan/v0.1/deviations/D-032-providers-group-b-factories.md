# D-032：providers group B 八工厂 Rust 落地差异（filterModels trait 落位、PendingOAuth 占位、cloudflare/radius 辅助移植）

> 复制本模板为 `D-NNN-<short-slug>.md` 并填写。登记说明见 [README.md](./README.md)。

- **状态**：已回写
- **关联任务**：T13（W4 阶段 2，group B）
- **级别**：实现细节偏离
- **发现日期**：2026-08-07

## 原文档约定

- 文档与章节：`docs/02-design.md` §3.4（Provider 层）；上游基准
  `packages/ai/src/providers/{github-copilot,openrouter,vercel-ai-gateway,cloudflare-ai-gateway,cloudflare-workers-ai,cloudflare-auth,cloudflare-stream,opencode,opencode-go,radius,radius-config}.ts` @ 0.82.1 (2efa728)
- 原文约定：§3.4 工厂逐上游文件移植；`filterModels` 为 `Provider` 可选方法
  （models.ts:111，消费者 `Models.getAvailable`）；copilot/openrouter/radius
  工厂构造期经 `lazyOAuth({ name, load })` 注册 OAuth 通道；radius 工厂返回
  字面量对象（持有 `models` 闭包状态与 `refreshModels`），非 `createProvider`。

## 实际实现与偏离原因

1. **`filterModels` 落位 `Provider::filter_models` trait 默认方法**（默认原样返回），
   github-copilot 以装饰器 `GithubCopilotProvider` 包裹 `create_provider` 输出并覆盖之，
   **不扩 `CreateProviderOptions` 字段**——W4 四个代理并行在共享 `models.rs`
   构造器上加必填字段会互相打破编译；trait 默认方法为纯增量。消费者
   `Models::get_available` 仍属 W5，现阶段 trait 方法经工厂直接可测
   （`tests/providers_group_b.rs::test_github_copilot_filter_models`）。
   `availableModelIds` 读取自 `OAuthCredential.extra["availableModelIds"]`
   （上游 `[key: string]: unknown` 展平字段的 Rust 对应物），非数组/非全字符串
   时目录原样返回，与上游守卫逐条对齐。
2. **OAuth 占位用具名 stub `PendingOAuth`**（`providers/pending_oauth.rs`），而非
   D-029/030/031 的 `oauth: None`：auth 表面保留上游 display name
   （"GitHub Copilot" / "OpenRouter OAuth" / "Radius"），`login`/`refresh`/
   `to_auth` 统一报「not ported yet (lands in T13 W5)」。差异仅形式——两组
   在 W5 前都不可用 OAuth 登录。openrouter 的 `loginLabel`
   （"Sign in with OpenRouter"）未移植：`OAuthAuth`/`AuthInteraction` 无对应槽，
   W5 UI 接线时补齐。
3. **cloudflare-auth.ts 落 `src/auth/cloudflare_auth.rs`**（两工厂共享的
   `ApiKeyAuth`，按任务书 auth helper 归 auth/）；workers-ai / ai-gateway 两 kind
   合一类型；JS falsy 语义显式化为空字符串过滤（`!apiKey` → 空串视为缺失）；
   ai-gateway 的 `Authorization: null` / `x-api-key: null` 映射为
   `ProviderHeaders` 的 `None` 值（headers.ts null 删除语义）。
4. **cloudflare-stream.ts 落 `src/providers/cloudflare_stream.rs`**；env 未解析
   占位符时克隆模型返回（上游返回同一引用，Rust 借用语义下等价——调用方拿到的
   都是与入参内容一致的所有权值）。
5. **radius 工厂 = `create_provider` 核心 + `RadiusProvider` 装饰器**：装饰器持有
   规范化 gateway URL（`gateway()` getter 为 W5 `refreshModels`/OAuth load 的
   接线点）；上游字面量对象持有的 `models`/`inflightRefresh` 闭包状态与
   `refreshModels` 整体属 W5 动态 overlay，W4 静态目录恒空（providers.test.ts:41
   已固化）。`radius_provider_with` 返回 `Arc<RadiusProvider>` 具象类型，
   `radius_provider()` 收敛为注册表签名 `fn() -> Arc<dyn Provider>`。
6. **radius-config.ts 落 `src/providers/radius_config.rs`**：上游
   `isRadiusGatewayModel`/`sanitizeRadiusGatewayConfig` 运行时 guard 保留同形
   预检，其后经 serde 物化——上游对未校验字段做 unchecked cast 透传，Rust
   直接构造 `Model`，不可表达的扩展字段不携带（上游 TS 结构类型下这些字段
   对 `Model` 消费者同样不可见，语义等价）；`truncateHttpBody` 按 Unicode
   scalar 计数（同 D-021 先例）；`AbortSignal` → `CancellationToken` +
   `tokio::select!` 竞速；fetch 网络错误经 `ModelsError::with_cause` 包装
   （上游为裸 throw，调用侧 W5 才落地）。

## W5 解决记录（2026-08-06）

- **第 2 项（OAuth 占位）已整体解决**：github-copilot / radius / openrouter 三
  流程已在 T13 W5 落地（`auth/oauth/github_copilot.rs` /
  `auth/oauth/radius.rs` / `auth/oauth/openrouter.rs`——openrouter 为 PKCE 换
  永久 key、refresh no-op，上游 `packages/ai/src/auth/oauth/openrouter.ts`
  @ 0.82.1 2efa728），三工厂的 OAuth 槽从 `PendingOAuth` 替换为真实流程
  （display name 不变）；`providers/pending_oauth.rs` 已删除（无剩余使用方）。
  openrouter 的 `loginLabel`（"Sign in with OpenRouter"）仍未移植：
  `OAuthAuth`/`AuthInteraction` 无对应槽，W5 UI 接线时补齐。
- **第 5 项（radius 装饰器接线点）部分解决**：`RadiusProvider::gateway()`
  已成为 OAuth 构造输入——`radius_provider_with` 以规范化 gateway 构造
  `RadiusOAuth`（`radius_config` 接线点闭环）；`refreshModels` 动态 overlay
  （`models`/`inflightRefresh` 闭包状态）仍属 W6 catalog overlay 范围，静态
  目录恒空的约定不变。
- **W6-C 解决记录（2026-08-06）**：**第 5 项已整体闭环**——`RadiusProvider` 增
  `models` 闭包单元 + `InflightRefresh` 去重槽，`refresh_models` 移植
  radius.ts:36-63（store 恢复 → legacy `gatewayConfig` 导入 → `{gateway}/v1/config`
  拉取并落 store，Bearer 用解析后凭据）；静态目录恒空约定不变（get_models 即闭包
  单元内容）。落地差异登记为 D-038。
- 两流程自身的落地差异（URL 重写测试缝、ring UUIDv4、poll 错误通道等）
  登记为 D-033；openrouter 流程的落地差异登记为 D-035。

## 影响面

无（纯内部）——不改变协议 / session 格式 / 扩展 API / TUI 行为契约；
OAuth 与 radius 动态目录为 W5 前的阶段性缺口，与 D-029/030/031 同性质。

## 处置

- **回写位置**：`docs/02-design.md` §3.4（W4 波次注记段落补 group B 注记）
- **回写日期**：2026-08-07
- **ADR**：不需要
