# 偏离管理（Deviations）

> 记录开发过程中实现与原始文档（`01-requirements.md` / `02-design.md` / ADR /
> `coding-standards.md`）之间的所有偏离。偏离必须**登记在此目录**并**回写原始文档**，
> 两者齐备才算闭环（门禁 G7）。

---

## 1. 偏离分级

| 级别 | 定义 | 处置 |
|------|------|------|
| **实现细节偏离** | 不影响行为契约：模块内部结构、私有 API、crate 内文件拆分、依赖选型微调 | 登记 + 回写原始文档，随任务门禁闭环 |
| **行为级偏离** | 影响对拍契约：事件序、线格式、session JSONL、compaction/token 估算、RPC 语义、TUI 行为、CLI/slash 行为 | **不允许直接落地**。须先立 ADR 转入「有意差异」（需求文档 §1.5 第 3 层），再登记回写 |

拿不准级别时按行为级处理。

## 2. 登记流程

1. 复制 [`TEMPLATE.md`](./TEMPLATE.md) 为 `D-NNN-<short-slug>.md`（NNN 从 001 起递增）；
2. 填写偏离内容，状态初始为 `待回写`；
3. 在下表登记一行；
4. 回写原始文档（在原文对应章节更新描述，保持文档与实现一致）；
5. 在偏离文件中填「回写位置」，状态改为 `已回写`；
6. 任务门禁验收时逐条核对（gates.md G7）。

## 3. 偏离登记表

| ID | 任务 | 级别 | 摘要 | 回写位置 | ADR | 状态 |
|----|------|------|------|----------|-----|------|
| D-001 | T01 | 实现细节 | session 条目类型单一来源化（`pir-agent::session`，合并 coding-agent 与 harness 两套定义） | `02-design.md` §4.1、§12 | 不需要 | 已回写 |
| D-002 | T01 | 实现细节 | TS 类型系统特性的 Rust 表达（声明合并折叠、compat 条件类型合并、AgentTool trait 化、Api 开放联合 newtype 化） | `02-design.md` §3.2、§4.1 | 不需要 | 已回写 |
| D-003 | T02 | 实现细节 | faux provider 确定性化（切块 / 默认 id / 默认 timestamp / 同步工厂；chars/4 usage 估算） | `02-design.md` §3.7、`fixtures/README.md` §2 | 不需要 | 已关闭 |
| D-004 | T03 | 实现细节 | ApiStream trait 形状 → ProviderStreams（同步返回事件流，含 stream_simple） | `02-design.md` §3.3 | 不需要 | 已回写 |
| D-005 | T03 | 实现细节 | 适配器 HTTP 层 reqwest 直连替代官方 SDK 的可观测差异（SDK 头/超时/严格 SSE 解析文案/metadata.raw 来源/错误前缀范围） | `02-design.md` §3.3 | 不需要 | 已回写 |
| D-006 | T03 | 实现细节 | 校验/解析层差异（jsonschema 单路径、models.json serde+手工 pass、错误措辞≠TypeBox） | `01-requirements.md` §5.5、`02-design.md` §3.6 | 不需要 | 已回写 |
| D-007 | T03 | 实现细节 | sanitize_surrogates 在 Rust 侧为恒等（String 无孤立代理） | `02-design.md` §3.6 | 不需要 | 已回写 |
| D-008 | T04 | 实现细节 | auth 存储与 key DSL 的 Rust 落地差异（fs2 无 stale/compromised、jitter 随机源、`!cmd` 仅 unix、快照保序方案、resolve_headers 形状） | `02-design.md` §3.5、`01-requirements.md` §5.4 | 不需要 | 已回写 |
| D-009 | T04 | 实现细节 | OAuth 框架的 Rust 落地差异（时钟抽象、测试缝、回调服务分支、错误明细近似、token JSON 严格化、竞速实现） | `02-design.md` §3.5、`01-requirements.md` §5.4 | 不需要 | 已回写 |
| D-010 | T05 | 实现细节 | agent_loop 与 Agent 的 Rust 落地差异（before/after 钩子回传与错误通道、流无终止事件合成、JoinError 合成、details null 省略、Message 错误变体、continue_run 命名等 11 项） | `02-design.md` §4.4 | 不需要 | 已回写 |
| D-011 | T06 | 实现细节 | 内置工具层 Rust 落地差异（ToolContext 形状、image/kamadak-exif 替代 Photon、自实现 Myers diff、OutputAccumulator 同步 API、on_data Vec<u8>、trackDetachedChildPid 未移植、~/.pir/bin 等 12 项） | `02-design.md` §6.5、`coding-standards.md` 附录 A | 不需要 | 已回写 |
| D-012 | T07 | 实现细节 | SessionManager 与路径模块 Rust 落地差异（retainedTail 展开采 session-format.md/harness 行为、随机源自实现、serde default 修正、list/listAll 留 T12、typed 联合体降级边界 4 项、同步 IO 等 9+2 项） | `02-design.md` §6.3、§8，`01-requirements.md` §6.6 | 不需要 | 已回写 |
| D-013 | T08 | 实现细节 | compaction 移植 Rust 落地差异（算法层落 pir-agent::compaction + 触发接线 pir::core::compaction_runner、StreamOptions.reasoning 字段、session 共享函数下沉 3 项） | `02-design.md` §4.4、§6.4、§12 | 不需要 | 已回写 |
| D-014 | T09 | 实现细节 | settings 与资源加载 Rust 落地差异（同步写盘/fs2 flock、Settings 保序 map 与类型收窄、serde_yaml/TypeBox/SyntaxError 引擎级文案、description 截断按 Unicode scalar、sourceInfo 归 resource_loader、TUI 件下沉 T11/T12、extensions/packages 占位边界等 11 项） | `02-design.md` §6.7、§12 | 不需要 | 已回写 |
| D-015 | T10 | 实现细节 | headless 模式 Rust 落地差异（clap→手写解析器、provider 生态 T13 子集、--resume picker/子命令/--export 占位、docs 路径=exe dir、session_env 动态 cell、资源枚举确定性排序、SessionManager::list 提前等 7 项） | `02-design.md` §6.1、§6.3、§6.6、§12 | 不需要 | 已回写 |
| D-016 | T11 | 行为级（功能缺口 2 条）+ 实现细节 | pir-tui 核心引擎 Rust 落地差异（macOS 原生修饰键检测缺失、Windows VT input 缺失两条功能缺口 → 目标平台 TUI 键位行为不同，判行为级理由见偏离文件「功能缺口」节；其余定时器显式 deadline、SharedComponent 与重入、组件实现细节等 31 项） | `01-requirements.md` §8.6、`02-design.md` §5、§12、`coding-standards.md` §8.2、`T11-pir-tui-core.md`、ADR-0004 | ADR-0004（行为级两条）/ 不需要 | 已关闭 |
| D-017 | T11 | 实现细节 | 终端恢复语义落位 pir-tui `recovery.rs`（上游在 coding-agent interactive-mode 层；panic 后恢复终端不退出进程 exit 101 vs 上游 exit 1；信号恢复 exit 0 对齐 `shutdown({fromSignal:true})`） | `02-design.md` §5.6、§12 | 不需要 | 已关闭 |
| D-018 | T12 | 实现细节 | Markdown 解析器 comrak 0.54 替代 marked@18.0.5（AST 对应：sourcepos 切片还原 raw、space token 合成、严格删除线/任务判定对齐；3 条残留边缘差异 + 3 个 xterm 用例改输出级断言） | `02-design.md` §5、§12、`01-requirements.md` §8.6 | 不需要 | 已回写 |
| D-019 | T12 | 实现细节 | interactive 模式移植 Rust 落地笔记（汇总型 25 条：组件 region 模式、显式主题注入、/copy 仅 OSC52、/debug 行段缺口、Ctrl+Z SIGTSTP、轮询主题/git watcher、willRetry 死代码、首启判定、--resume 独立 picker、OutputPad streaming 等，逐条三档标注；「会话切换不重订阅」2026-08-06 修复关闭） | `02-design.md` §5、§12、`01-requirements.md` §8、`T12-interactive-mode.md` | 不需要 | 已回写 |
| D-020 | T16 | 实现细节 | harness 层 Rust 落地差异（**harness compaction 变体勘误**：prepareCompaction 与 coding-agent 版不同，harness 变体移植于 agent_harness.rs；SessionStorage 写方法 &self+Mutex；SessionManager build_index leaf 重放兼容；skills/templates/system-prompt 独立移植；pir-agent 新增 6 基线依赖；env/tools/truncate/proxy 局部等价 8 组） | `02-design.md` §6.4、§12、`T16-agent-harness.md` | 不需要 | 已回写 |
| D-021 | T13 | 实现细节 | pi-messages 适配器 Rust 落地差异（streamSimple 的 toolChoice/debug 走私字段未移植、SSE 解析失败文案 serde_json 化、稀疏数组/松散强转语义差、statusText 取 canonical reason、`PI_CACHE_RETENTION`→`PIR_CACHE_RETENTION` 且不设默认、截断按 Unicode scalar、lazy.ts 无对应物、body 空分支不移植） | `02-design.md` §3.3 | 不需要 | 已回写 |
| D-022 | T13 | 实现细节 | mistral-conversations 适配器 Rust 落地差异（reqwest 直连无 SDK 头/超时、on_payload 见 snake_case wire JSON、严格 serde SSE 解析文案、错误 fallback 文案、x-affinity 大小写不敏感覆盖检查、stripSymbolKeys 恒等、partialArgs 侧置 scratch、重试走共享 helper 默认 0、lazy.ts 无对应物） | `02-design.md` §3.3 | 不需要 | 已回写 |
| D-023 | T13 | 实现细节 | google-generative-ai 适配器 `@google/genai` SDK 反推与 reqwest 直连差异（反推线格式：`POST {baseUrl}/models/{id}:streamGenerateContent?alt=sse` + `x-goog-api-key` + generationConfig 拆分；SDK 遥测头不发、SDK 默认无重试故 max_retries 无效、SseDecoder 替代分隔符切分器、严格 serde SSE 解析文案、错误信息=throwErrorIfNotOK 的 body 序列化、chunk 级流内错误探针、usage.input 负值饱和、on_payload 保持 SDK 层 params 形状） | `02-design.md` §3.3 | 不需要 | 已回写 |
| D-024 | T13 | 实现细节 | azure-openai-responses 适配器 Rust 落地差异（reqwest 直连替代 `AzureOpenAI` 客户端：线格式经 openai@6.26.0 源码核对 `POST {baseUrl}/responses?api-version={v}` + `api-key` 头、deployment 在 body `model`、SDK 遥测头/默认超时不发、on_payload 见 snake_case wire JSON、严格 serde SSE 解析文案、错误前缀适用范围与上游一致、streamSimple 缺 key 进事件流、lazy.ts 无对应物） | `02-design.md` §3.3 | 不需要 | 已回写 |
| D-025 | T13 | 实现细节 | google-vertex 适配器 SDK 反推与 ADC 子集自实现差异（反推线格式：`POST {base}[/{apiVersion}][/projects/{p}/locations/{l}]/publishers/google/models/{id}:streamGenerateContent?alt=sse`，自定义 baseUrl COLLECTION 作用域、global/多区域/区域端点选择；`x-goog-api-key` 或 `authorization: Bearer`；ADC 链=GOOGLE_APPLICATION_CREDENTIALS→well-known→metadata，支持 service_account JWT-bearer+authorized_user refresh，external_account/impersonated 显式报错缺口；token 端点 v10 起不读 token_uri；metadata 请求兼作 3s 探测；SDK 遥测头不发、max_retries 无效、SseDecoder/严格 serde 文案、on_payload SDK 层形状、ring/base64 进 pir-ai、vertex thinking 表无 gemma4/flash-lite、stream_simple 不预检 key、lazy.ts 无对应物、AdcEndpoints/resolve_request_url 测试缝） | `02-design.md` §3.3、`coding-standards.md` 附录 A | 不需要 | 已回写 |
| D-026 | T13 | 实现细节 | bedrock-converse-stream 适配器 `@aws-sdk`/`@smithy` 反推与 reqwest 直连差异（反推线格式：`POST {endpoint}/model/{enc(modelId)}/converse-stream` + rest-json1 body；手写 SigV4 对齐 `@smithy/signature-v4`（含 x-amz-content-sha256 签名、canonical path 二次编码、service=bedrock）；event-stream 帧解码对齐 smithy codec；SDK 头不发、重试走共享 helper 由 max_retries 驱动、凭据链仅 env（profile/SSO/IMDS 缺口、profile 选项 inert）、ambient-profile 无 region 时兜底 us-east-1、endpoint ruleset 收敛为区域标准域名、proxy/FORCE_HTTP1 未移植、on_payload 带 modelId 的 wire JSON、图像 base64 直通、PIR_CACHE_RETENTION、lazy.ts 无对应物、新增 sha2/hmac 依赖） | `02-design.md` §3.3、`coding-standards.md` 附录 A | 不需要 | 已回写 |
| D-027 | T13 | 实现细节 | openai-codex-responses 适配器 Rust 落地差异（WS 状态机表达：socket 移出 entry 代 busy 标志、spawn+代际计数代 setTimeout、非阻塞 poll 探针代 readyState 并存 pending 帧；reqwest/tokio-tungstenite 直连无运行时探测分支；SSE 体超时文案、zstd 恒压缩、JWT 多字母表、UA 用 consts+libc uname、CodexError 枚举分类、session_resources 不可失败 fn 指针、SseDecoder 共享语义、stream_simple 缺 key 进事件流、lazy.ts 无对应物、openai-beta 删除 no-op bug-compatible、TTL 测试缝参数化、新增 tokio-tungstenite/zstd/libc 依赖） | `02-design.md` §13、`coding-standards.md` 附录 A | 不需要 | 已回写 |
| D-028 | T13 | 实现细节 | 内置模型目录管线与注册表骨架 Rust 落地差异（include_str!+运行时 serde 惰性解析代 TS 字面量 codegen、修正规则生成期烘焙于 vendored JSON 不重放、38 工厂 spec 表骨架期内 `builtin_providers()` 产子集、按需子集不用 feature flags、generatedAt 手写 ISO 解析；附注：目录实为 37 JSON+manifest 非任务书 30 份） | `02-design.md` §3.4、§12 | 不需要 | 已回写 |
| D-029 | T13 | 实现细节 | kimi-coding 工厂 OAuth 槽 W4 阶段占位（上游构造期 `lazyOAuth` Kimi Code 订阅登录属 W5 范围，W4 以 `oauth: None` 落地、`ProviderAuth.oauth` 槽为接线点；W5 已接线关闭） | `02-design.md` §3.4 | 不需要 | 已关闭 |
| D-030 | T13 | 实现细节 | openai-codex 工厂 auth W4 阶段占位（上游为纯 OAuth 工厂、无 api-key 通道，OAuth 属 W5 范围，W4 以空 `ProviderAuth` 落地——工厂/目录/适配器接线就位但 auth 恒未配置；W5 已填入 `openai_codex_oauth()` 关闭） | `02-design.md` §3.4 | 不需要 | 已关闭 |
| D-031 | T13 | 实现细节 | xai 工厂 OAuth 槽 W4 阶段占位（上游构造期 `lazyOAuth` SuperGrok/X Premium 订阅登录属 W5 范围，W4 以 `oauth: None` 落地、`ProviderAuth.oauth` 槽为接线点，仅 `XAI_API_KEY` api-key 通道；W5 已接线关闭；同 D-029 模式） | `02-design.md` §3.4 | 不需要 | 已关闭 |
| D-032 | T13 | 实现细节 | providers group B 八工厂 Rust 落地差异（`filterModels` 落 `Provider::filter_models` 默认方法 + copilot 装饰器、不扩 `CreateProviderOptions`；OAuth 用具名 `PendingOAuth` stub 占位——区别于 D-029/030/031 的 `oauth: None`，openrouter `loginLabel` 未移植；cloudflare-auth 两 kind 合一、空串按 JS falsy 过滤；radius=create_provider 核心+装饰器持有规范化 gateway、refreshModels/OAuth 属 W5；radius-config guard+serde 组合、truncateHttpBody 按 Unicode scalar、AbortSignal→CancellationToken）——W5 解决记录：copilot/radius/openrouter OAuth 已落地，`PendingOAuth` 已删除，openrouter `loginLabel` 仍缺槽位 | `02-design.md` §3.4 | 不需要 | 已回写 |
| D-033 | T13 | 实现细节 | github-copilot / radius OAuth 流程 Rust 落地差异（copilot URL 重写测试缝替代全局 fetch 打桩、radius 回调服务独立 axum 实现且 REDIRECT_URI 保持上游常量、ring UUIDv4 代 crypto.randomUUID、poll 闭包抛错→Failed 同文案、statusText canonical reason、number→f64 窄化、enableAll=join_all 吞错、credential extras 存在才写入） | `02-design.md` §3.4、§3.5 | 不需要 | 已回写 |
| D-034 | T13 | 实现细节 | kimi-coding / xai OAuth 流程与 load.ts 对应物 Rust 落地差异（构造字段测试缝代 fetch/env 打桩、请求期取消 → "Login cancelled"、kimi client 级 30s 超时 / xai 无超时、poll 抛错 → Failed 同文案、readJson 的 typeof-object 数组语义、load.ts → `load.rs` registry 函数表、refresh 背退不可中断等） | `02-design.md` §3.4、§3.5 | 不需要 | 已回写 |
| D-035 | T13 | 实现细节 | openai-codex / openrouter OAuth 流程 Rust 落地差异（authority URL 重写 + callback_port / token_url 测试缝、回调服务器 axum 化与超时竞速、atob 宽松 JWT 解码四字母表、poll 抛错→Failed、token JSON 严格化、openrouter `Number.MAX_SAFE_INTEGER` 精确值、refresh no-op、竞速回退语义） | `02-design.md` §3.4、§3.5 | 不需要 | 已回写 |
| D-036 | T13 | 实现细节 | W6-B 横切能力：上游 `cross-provider-handoff.test.ts` 为 live 测试（须真实 API keys，`skipIf(!hasAnyApiKey())`）不移植，意图由 transform_messages 全规则 + 六适配器 normalize 回调纯函数测试覆盖；另记录 `transform-messages.ts` null content 归一化由 serde 边界容忍（types.rs `null_default`，T03 已落地） | `02-design.md` §3.6 | 不需要 | 已回写 |
| D-037 | T13 | 实现细节 | image generation 子系统 Rust 落地差异（文件聚合于 `images.rs` + `images/`；`ImagesApiKind` newtype 与 `ImagesModel` 泛型折叠；`ProviderImages` trait / `ImagesFunction` Arc<dyn Fn> + BoxFuture；无 import 副作用 → 首次 dispatch 前惰性注册、`createLazyLoadErrorImages` 死代码；dispatch 抛错 → `Result`、wrap 不匹配检查以 error 结果表达；reqwest 直连替代 openai SDK（错误文案遵循 D-005 组合、无 metadata.raw）；响应解析容忍度对齐上游未加保护读取；永不 reject 双层 catch；refresh 错误恒包 `model_source`（无直通分支）、全量 refresh join_all 并发；`get_models` try/catch 无对应；目录由 node 转写 40 模型（OnceLock）、generate-image-models 脚本不移植、`image-model-data.test.ts` 意图以目录校验测试表达；`ProviderImagesOptions` 额外键丢弃；lazy.ts 无对应物；时间戳/取消竞态/on_payload wire JSON 说明） | `02-design.md` §3.3、§3.6、§12 | 不需要 | 已回写 |
| D-038 | T13 | 实现细节 | 远程模型目录 overlay 与 `Models::refresh` Rust 落地差异（`createProvider.fetchModels` 钩子延后 T15 不扩 `CreateProviderOptions`；`Provider::refresh_models` 以 `Option<BoxFuture>` 表达可选方法 + probe 过滤；`with_remote_catalog` 落 `crates/pir/src/core/remote_catalog_provider.rs` 并新增 reqwest/httpdate/url 依赖、UA 以 `rust` 标记 runtime 分量；`parseCatalog` 丢 serde 不可表达条目；store 错误统一 `model_source` 映射；`models-store.json` 损坏回退内存存储；ModelRuntime 未注册内置 provider（注册波次按 model-runtime.ts:144-150 包装）故装饰器无运行时消费者、compose/overlay 预留 refresh 委托；`update --models` 显式 allow_network 无视 PIR_OFFLINE；测试基建：上游 fetch 打桩 → loopback 脚本化 HTTP 服务器；15s 超时收敛为常量并注入短超时测试） | `02-design.md` §3.4、§12、D-032 解决记录 | 不需要 | 已回写 |
| D-039 | T14 | 第 1 条行为级 / 余实现细节 | 可选工具 grep/find/ls Rust 原生落地差异（ignore/globset 替代外部 rg/fd，ADR-0003 §2 授权路线；ls 排序 codepoint 替代 ICU localeCompare；regex/globset 原生错误文案替代 rg/fd stderr 透传；grep 二进制判定全文 NUL 扫描；`--glob` 锚点取会话 cwd；取消按 walk 条目检查；find 对 node_modules/.git 整目录剪枝；find custom-ops 相对化回退简化） | `01-requirements.md` §4.5 | ADR-0005（第 1 条） | 已关闭 |
| D-040 | T14 | 实现细节 | Packages 包管理核心 Rust 落地差异（hosted-git-info 五 host 子集自实现 + url crate；semver crate + npm range 翻译层语义边界；PackageCommandRunner 注入与引擎级错误文案；legacy npm root 无缓存；resolve 仅包切片；settings 畸形 packages 项重写丢弃；list 忽略位置参数 quirk 保留；headless 信任链等 13 项；终审补记 3 条：PIR_OFFLINE 采 main.ts isTruthyEnvFlag 语义、getNpmInstallPath 吞 trust 错误、display() 脱敏 URL userinfo） | `01-requirements.md` §7.6 | 不需要 | 已关闭 |
| D-041 | T14 | 实现细节 | update 编排/自更新/版本检查 Rust 落地差异（scoped-thread worker pool 并发 4；补齐 W6-C 漏掉的 `--all` 冲突两条；自更新按上游包管理器重装机制移植、原生二进制按 bun-binary 结局；PACKAGE_NAME/LATEST_VERSION_URL/SELF_UPDATE_DOWNLOAD_URL 集中常量留 W6 口子；HTTP 注入；runner 引擎级文案；resolve 包切片 canonical 去重改按类型等 11 项） | `01-requirements.md` §7.6 | 不需要 | 已关闭 |
| D-042 | T14 | 实现细节 | config 子命令与 config-selector 真接线差异（组件输入换 `ScopedResolvedPaths` + `resolve_all` 全量解析；settings 持久化逐函数移植；项目视图用同文件新建受信 manager；ctrl+c→onExit 上游死代码 quirk 保留；T12 写钩子删除；TUI 驱动复用 session-picker 模式等 7 项） | `01-requirements.md` §3.2 | 不需要 | 已关闭 |
| D-043 | T14 | 实现细节 | Project trust 产品化收尾 Rust 落地差异（`ProjectTrustContext` 闭包化仅留 select；resolve 保持同步、扩展事件预发射为参数；启动弹窗 `run_startup_selector` 复用 ExtensionSelectorComponent + 泵线程同步等结果；`getProjectTrustOptions` 落核心层；弹窗文案 pi→pir；switchSession 异 cwd 提示已析出为 D-044） | T14 偏离表 | 不需要 | 已关闭 |
| D-044 | T14 | 行为级 | 交互模式 switchSession 异 cwd 信任提示降级为 headless 判定（无 TUI 弹窗，ask→false + untrusted warning，`/trust` + 重启生效；同 cwd 缓存路径与上游一致；**T15 W7 已接线异步信任选择器并补测试，偏离消除**） | T14 偏离表、D-043 遗留项、D-044 关闭记录 | ADR-0006 | 已关闭 |
| D-045 | T14 | 实现细节 | HTML export / gist share Rust 落地差异（模板资产 include_str! 内嵌；renderedTools/ANSI→HTML 管线不移植；theme vars 按键排序；去 currentThemeName 全局；export_to_html 同步化；ShareRunner 注入 + UiCommand drain 结算；PIR_SHARE_VIEWER_URL 集中 config） | `02-design.md` §12 + §6.1 注记、T14 偏离表 | 不需要 | 已关闭 |
| D-046 | T14 | 实现细节 | 产品 endpoint 配置化与 install telemetry Rust 落地差异（统一 resolve_endpoint env>settings>默认/`off` 关闭；三个 PIR_*_URL + 三个 camelCase settings 键为 pir 专有；update 流程与启动版本检查接线；reportInstallTelemetry 移植、changelog 触发以版本不等近似待 T15；enableAnalytics 无发送通道按构造零请求；catalog 解析器就位待注册波次；测试 PIR_* 只读不写纪律） | `02-design.md` §8、§12、T14 偏离表 | 不需要 | 已关闭 |
| D-047 | T14 | 实现细节 | llama.cpp 集成与 /login api-key 通路 Rust 落地差异（内置 hidden 扩展登记表 + dispatch fall-through 替代 T15 宿主；provider 进程级 OnceLock 单例与 services drain 注册；LlamaView 双半 + oneshot 解析；CancellationToken/select!/generation debounce；reqwest 连接错误分类；Models/ModelRuntime login/logout/get_provider_auth 补齐、ApiKeyAuth::supports_login 标志；OAuth 对话框仍 stub 属 T13 遗留。**T15 W7 已迁移为经真宿主加载的内置 hidden 扩展，临时机制全部移除**，见 D-047「T15 W7 迁移记录」） | `docs/llama-cpp.md`、`01-requirements.md` §6.4/§9、T14 偏离表 | 不需要 | 已关闭 |
| D-048 | T15 | 第 2、6 条行为级 / 余实现细节 | 扩展宿主核心动作与事件落地差异（汇总型 6 条：tool_call 改参经结果 `input` 穿线替代共享可变 event；**user_bash `operations` 闭包束不支持丢弃回退**；registerProvider 闭包子项 streamSimple/oauth/refreshModels 显式拒绝；newSession `setup` 回调省略以 withSession 替代；exec 超时直接 SIGKILL 无 SIGTERM 升级；**非 RPC 模式 ctx.command.* 未绑走上游默认值**） | `02-design.md` §7.2、`extension-abi.md` §3、T15 偏离表 | ADR-0007（第 2、6 条） | 已回写 |
| D-049 | T15 | 第 1 条行为级 / 余实现细节 | 扩展 UI/渲染层落地差异（**custom() 声明式 v1 无交互回传、展示后立即 resolve undefined**——交互需求走 UiBridge::as_any 原生口子；ComponentTree schema v1 无 `row` 横向容器、未知 type fail-visible 渲染） | `02-design.md` §13、`extension-abi.md` §7、T15 偏离表 | ADR-0007（第 1 条） | 已回写 |
| D-050 | T15 | 实现细节 | L0 原生动态库插件（abi_stable 0.11.3）落地差异（ABI 形状收敛：PirHostCalls 结构体按值传 host-call 句柄、cookie=*const c_void、RVec<u8> 拥有型缓冲；无沙箱信任模型明示——capability 只管扩展 API 面；manifest 新增 `native` 字段与 wasm 互斥、wasm 优先；无 fuel/Store/专属线程） | `extension-abi.md` §1.1/§5、`02-design.md` §7.2、T15 偏离表 | 不需要 | 已回写 |

## 4. 状态定义

| 状态 | 含义 |
|------|------|
| `待回写` | 已登记，尚未回写原始文档 |
| `已回写` | 已回写原始文档，待任务门禁确认 |
| `已关闭` | 门禁验收通过，偏离闭环 |
