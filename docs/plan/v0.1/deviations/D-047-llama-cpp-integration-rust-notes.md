# D-047：llama.cpp 集成与 /login api-key 通路 Rust 落地差异（T14-W6b）

> 复制本模板为 `D-NNN-<short-slug>.md` 并填写。登记说明见 [README.md](./README.md)。

- **状态**：已关闭
- **关联任务**：T14（W6b）
- **级别**：实现细节偏离
- **发现日期**：2026-08-07

## 原文档约定

- 文档与章节：`external/pi/packages/coding-agent/docs/llama-cpp.md`（产品口径）、
  `docs/01-requirements.md` §6.4（`/llama` 内置 hidden 扩展注册，非内置命令）、§9（内置
  hidden llama.cpp 扩展加载）、§8.4 相关项（动态 catalog）
- 原文约定：llama.cpp 以**内置 hidden 扩展**交付——`/llama` 命令经扩展注册；provider 经
  `pi.registerProvider(providerObject)` 注册；HF 搜索/下载 `owner/repo[:quant]`、HF_TOKEN
  查找链；永不静默卸载/删除；`/login llama.cpp` 与 `LLAMA_BASE_URL`/`LLAMA_API_KEY`。

## 实际实现与偏离原因

T15（扩展宿主 L0+L1）未开始，扩展注册/自定义 UI/命令表等宿主机制缺位。本波次按「最小侵入
等价物」接线，差异如下（均不改变产品行为契约）：

1. **内置 hidden 扩展登记表替代扩展宿主**：`crates/pir/src/extensions/mod.rs` 的
   `BUILT_IN_EXTENSION_COMMANDS`（对应 `extensions/index.ts` 的 `builtInExtensions`）。
   `/llama` 在交互模式 `dispatch_slash_command` 的内置命令未命中分支查表分发；上游是落入
   `session.prompt` 的扩展命令路径（interactive-mode.ts:4017 `isExtensionCommand`）由
   extension runner 执行。autocomplete 的扩展命令段（interactive-mode.ts:599-608）由该表
   直供（hidden 内置扩展无 source tag，description 不加前缀）。T15 就位后此表应迁移到真
   正的扩展命令注册。
2. **provider 进程级共享实例**：上游扩展工厂闭包持有 `createLlamaProvider()` 实例
   （`setCatalog` 控制器随扩展加载唯一）；pir 以 `OnceLock` 进程级单例
   （`shared_llama_provider()`）替代，注册点固定在 `create_agent_session_services`（对应
   agent-session-services.ts:166-178 的 `pendingNativeProviderRegistrations` drain），
   `/llama` 命令处理器共享同一实例。目录内容始终来自同一路由器，语义等价。
3. **`/llama` TUI 挂载**：上游 `ctx.ui.custom(...)` 挂载自定义组件（T15 宿主钩子）；pir
   复用 selector 挂载机制（`show_selector` + `FocusableRegion`），组件与异步流程经
   `Arc<Mutex<LlamaViewState>>` + 每请求 oneshot 通道通信（`LlamaViewComponent` /
   `LlamaViewUi` 两半）。`showLlamaUi` 的错误边界 = 流程返回 `Err` 时 notify + 卸载。
4. **取消/并发原语**：`AbortSignal` → `CancellationToken`；15s `AbortSignal.timeout` →
   reqwest client 级 timeout；`runWithProgress` 的 settled 轮询 → `tokio::select!` 循环
   （进度重绘经通道转发——上游在进度回调里同步调 `updateProgress`，pir 的回调跑在 watcher
   任务上）；HF 搜索 500ms `setTimeout` debounce → spawn 任务 + generation 计数（陈旧结果
   丢弃规则与上游 `this.query !== query` 一致）。
5. **连接错误分类**：上游按 undici 错误文案匹配（`fetch failed`/`timeout`/`network`，
   index.ts:11-15）；pir 用 reqwest 的 `is_connect`/`is_timeout` 分类（`LlamaError.
   connection`），并保留文案子串检查兜底。
6. **`/login` api-key 通路接线**（T13 遗留 stub 的补完，interactive-mode.ts:4888-5312）：
   - `Models::login`/`logout`（models.ts:431-452）与 `ModelRuntime::login`/`logout`
     （model-runtime.ts:503-514）移植；`Models::get_provider_auth(provider_id)` 补齐
     `getAuth` 的 string 重载臂（llama `configuredClient` 所需）。
   - 「方法存在性」检查（上游 `method?.login`）无法直接表达——pir 的 `ApiKeyAuth::login`
     有默认错误实现，故 trait 增加 `supports_login()`（默认 `false`，覆盖 `login` 的实现
     必须返回 `true`）。已核对全部覆盖 `login` 的实现（anthropic/cloudflare/helpers/
     bedrock/vertex/llama）均正确覆盖。
   - `LoginDialogComponent` 增加 select prompt 能力；`AuthInteraction` 适配器把对话框
     prompt/通知桥接给 provider login。
   - OAuth 登录对话框流仍为 stub（`start_provider_login` 的 oauth 臂，显示未可用提示）—
     属 T13 遗留（T13 报告遗留项 5 已登记），非本波次范围。
7. **环境/Home 注入**：`findHuggingFaceToken` 收注入的 env map 与 home 目录（测试用临时
   HOME）；生产调用点传 `process_env()`/`default_home_dir()`。
8. **细节近似**：`localeCompare` → 普通字符串序（模型 id/quant 名为 ASCII）；HF details
   的 `file.size` 按整数读取（上游 `typeof === "number"`，字节数实践中恒为整数）；
   reqwest 响应体消费顺序要求先取 rate-limit 响应头再解析 JSON（上游 fetch 无此约束）。

无凭据落日志：HF_TOKEN/API key 仅作请求头使用；`LlamaError` 文案不含凭据（transport 错误
只带类别信息）。

## 回写位置

- 本表（deviations/README.md）D-047 行
- `docs/plan/v0.1/T14-packages-trust-export.md` 偏离记录表
- `docs/01-requirements.md` §6.4 / §9 相关条目（T14 任务回写时）

## 关闭条件

T15 扩展宿主就位后：迁移 `/llama` 到真正的扩展命令注册与 `ctx.ui.custom` 挂载，移除
`BUILT_IN_EXTENSION_COMMANDS` 直供与 `shared_llama_provider` 单例（或证明等价后保留）。

## 终审补记（2026-08-07）

- **HF 搜索缓存不跨进入存活**：上游 `searchCache` 挂在 view 实例上；pir 的缓存读自
  `state.content` 的 `Search` 变体，而进入搜索前 `show_models` 恒将 content 置为
  `Models`，故缓存实际每次为空——重复搜索会重新请求网络（行为正确，纯性能差异；
  独立抽查结论：无阻断/应修项，HF_TOKEN 仅进 Authorization header、删除/卸载均有
  确认、下载取消链路完整）。

## 审查修复补记（2026-08-07 审查修复波次）

1. **SSE watcher 超时修复（对应第 4 条的边界补完）**：reqwest client 级 `timeout` 是
   请求开始到响应体读完的总时长，原实现 `watch()` 复用带 15s 超时的 client，长于
   15s 的 load/download 会失去 SSE 进度事件（操作本身经轮询仍正确）。现 `LlamaClient`
   双 client：普通请求仍走 15s 超时 client（上游 per-request
   `AbortSignal.timeout(15000)`），`watch()` 走无总超时的 `stream_http` client
   （上游 watch 无超时，client.ts:213-245）。
2. **SSE 帧解析改字节级累积**：原实现逐块 `from_utf8_lossy`，多字节字符跨 TCP 块时
   损坏为 U+FFFD → JSON 解析失败 → 偶发丢事件（轮询自愈）。现按 `\n\n` 边界累积
   原始字节、整帧解码（CR 字节在字节级剥离，等价原 `\r\n→\n` 归一化）。
3. **watcher 复用 client**：`load_and_wait`/`download_and_wait` 的 SSE watcher 任务
   原每次重建完整 `LlamaClient`（新连接池）；现克隆宿主 client（含无超时 SSE client）。
4. **测试文案更正**：「17 个 loopback 集成测试」中
   `parse_hugging_face_model_splits_quant` 为纯单元测试（上游同名测试同文件），
   准确口径为 16 loopback + 1 单元。
5. **`EXACT_MODEL_PATTERN` 提为 `LazyLock` 编译一次**（原每次 confirm 重编译正则）。

## T15 W7 迁移记录（2026-08-08）

关闭条件兑现：llama.cpp 已迁移为经真扩展宿主加载的内置 hidden 扩展，
本文件第 1/2/3 条描述的临时机制全部移除：

1. `BUILT_IN_EXTENSION_COMMANDS` 直供表删除；`crates/pir/src/extensions/
   llama/mod.rs` 新增 `inline_extension()`——`Named { name: "llama.cpp",
   hidden: true }` 的内联扩展，factory 内经宿主 API
   `register_native_provider` + `register_command("llama")` 注册，两阶段
   启动均经 `builtin_extensions` 走标准加载路径。interactive dispatch 删
   `/llama` 特例（走 prompt 扩展命令路径），autocomplete 改用
   `runner.registered_commands()`。
2. `shared_llama_provider` 进程级单例与 `agent_session_services.rs` 的强制
   注册块删除；provider 由扩展 factory 注册，`app.rs` 在
   `create_agent_session` 前冲刷 `take_pending_native_provider_registrations`
   进 model_runtime（修初始模型可见性）。
3. `/llama` TUI 挂载改经宿主 `ctx.ui()`（`UiBridge::as_any` 新增 downcast
   口子）→ `InteractiveUiBridge` → `InteractiveUi::handle_llama_command`，
   LlamaView 组件机制本身不变。
4. 证据测试：`extensions/mod.rs` 新单测经真宿主验证注册（命令 + provider
   均经宿主 API 可见）；17 个 llama_extension loopback 测试保持绿色。

第 4–8 条（取消/并发原语、连接错误分类、/login 通路、env 注入、细节近似）
为 llama 集成自身的落地差异，与宿主迁移无关，维持原登记。
