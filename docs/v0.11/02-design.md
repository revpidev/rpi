# Pir v0.11 架构设计文档（对齐 Pi v0.84.1）

> 本文档是 [`../02-design.md`](../02-design.md)（v0.1 基线）的**增量设计**，描述 pir 从 Pi v0.82.1 升级到 v0.84.1+ 基线的架构变更。
> 对照源：`external/pi` @ `4181f66`。需求条目引用 [`01-requirements.md`](./01-requirements.md) 的 R 编号。
> 设计原则沿用 v0.1 §1；上游钉死更新为 `4181f66`（见 [`UPSTREAM.md`](../../UPSTREAM.md)）。

---

## 1. 升级策略

### 1.1 总方针

1. **基准唯一**：以 `4181f66`（v0.84.1+）为唯一对拍基准。上游在周期内回退重做过 fullscreen（`b70c0f5b4` revert），中间版本一律不参考。
2. **顺序：先协议后表现**：先落地线格式/消息类型变更（R2.1、R3.1）恢复对拍绿，再做行为修正（R2.3/R2.4、R3.4），最后做 fullscreen TUI 大工程（R3.2、R5.2）。
3. **不追过渡态**：上游 harness v2 运行时未实现且 record 契约将被 D0 重写——pir-agent harness 保持 v1 语义，不为 scaffold 写代码（需求 §1.2）。
4. **渲染基线重录前置**：R5.3.1（输入即时渲染）使逐帧黄金文件失效，在 TUI 动工第一天统一重录，避免新旧基线混杂。

### 1.2 变更- crate 映射总览

| crate | 主要变更 | 需求条目 | 风险 |
|-------|----------|----------|------|
| `pir-ai` | 类型字段扩展、流终止语义、provider 修复、models refresh 事务化、OAuth 行为、2 个新 provider | R2.1–R2.8 | 中（点状多、无架构变化） |
| `pir-agent` | Agent 循环 4 项微行为、CompactResult 契约、proxy 帧 | R4.1–R4.2 | 低 |
| `pir-tui` | **渲染器 trait 化重构** + 全屏子系统 + 布局引擎 + LaTeX + 宽度算法 | R5.1–R5.5 | **高（本版本最大工程）** |
| `pir` | JSON/RPC 线格式 + backpressure、会话行为簇、auth 命令、settings 深合并、资源/包管理 | R3.1–R3.7 | 中 |
| `pir-ext-sdk` / `pir-ext-host` | 扩展 API 面 12 项同步 | 需求 §6 | 中（ABI 兼容决策） |

---

## 2. pir-ai 设计（R2.1–R2.8）

### 2.1 类型扩展（`types.rs`）

```rust
// StopReason 新增
enum StopReason { Stop, Length, ToolUse, Error, Aborted, Pending, Deferred }

// AssistantMessage 新增（serde skip_serializing_if = None，与上游 optional 对齐）
struct AssistantMessage {
    // ...既有字段
    raw_stop_reason: Option<String>,
    end_turn: Option<bool>,
    deferred: Option<DeferredHandle>,   // 仅类型占位，见 R2.2.1
}

// ToolCall 新增
struct ToolCall { /* ... */ namespace: Option<String> }
```

- `DeferredHandle` 按上游形状定义（provider/model_id/api/id/expires_at/poll_after_ms/data），`data` 为 `serde_json::Value`。不实现 fetch/cancel 生命周期（R2.2.1 [DEFER]）。
- `Model`/`StreamOptions` 新增 `sampling_params: Option<Map<String, Value>>`，请求体组装**最后一步** `merge`（键覆盖命名参数），仅 OpenAI-compatible 适配器消费。
- `StreamOptions` 拆分为 `ProviderRequestOptions`（signal 等价物用 `CancellationToken`、telemetry_context 占位、api_key、自定义 fetch、headers、timeout、retry）+ `StreamOptions`（R2.8.1）。pir 已有 `CancellationToken` 惯例，无 signal 概念问题。

### 2.2 流终止语义（providers/*）

- 各 provider 的 stop-reason 映射函数返回结构从 `StopReason` 改为 `(StopReason, Option<String>)`（错误文案随映射产出），未映射 reason 走 `"Provider stopped with: <reason>"` / `"Response incomplete: <reason>"`（R2.3.1）。
- Responses 适配器：`incomplete_details.reason == "max_output_tokens"` 是 length 的**唯一**来源。
- completions 适配器新增 compat 标志：`supports_finish_reason`、`supports_thinking_token_budget`（+ `MIN_ANSWER_TOKENS = 1024` 预留）、`supports_additional_tools`、`chat_template_args`；`use_max_tokens` 名单加 DeepSeek/Z.AI 判定。
- 流式修复按 R2.4 逐条落入对应 provider 模块；每条配一个 golden 测试（移植上游期望值）。

### 2.3 Mistral 原生传输

- 删除对 mistral SDK 等价物的依赖，直接基于 pir 既有 SSE 解析基建实现 `mistral` 模块：自解析 `data:`/`[DONE]`/多行 JSON；`to_mistral_wire_payload()` 做 camelCase→snake_case 映射；保留 `x-affinity` 头逻辑。以 `mistral-http-transport.test.ts`（427 行）为对拍蓝本。

### 2.4 Models refresh 事务化（`models.rs` / `models_store.rs`）

- `refresh()` 改两阶段：phase 1 无条件 restore（先于 auth 解析）；phase 2 按需 fetch。
- 引入 **generation 计数器**：每次 `set_provider/delete_provider/clear_providers/refresh` 递增并 abort 上一代；发布走 `publish_provider_models()` 检查 generation，旧代丢弃；per-provider 发布串行化（`HashMap<ProviderId, JoinHandle>` 链）。
- `RefreshModelsContext`：`store` 字段删除，改为 `stored: ModelsStoreEntry`（只读快照）+ `publish(Publish { persist, update })`；`persist: None` = 不持久化、`Some(None)` = 删除条目。
- 调用方传入取消令牌时 `refresh()` 返回 `RefreshResult { aborted: bool, errors: Map }`。

### 2.5 OAuth（`auth/`）

- 刷新判定从"已过期"改为"剩余 < 5 分钟"；`min_oauth_validity_ms` 可选覆盖。
- 刷新操作包 15s 超时（`tokio::time::timeout` 与调用方取消 select）。
- `InMemoryCredentialStore` 等价物：排队等待被取消时立即拒绝，队列尾不阻塞后续。

### 2.6 新 provider

- `providers/baseten.rs`、`providers/qwen_token_plan_individual.rs`，模型目录数据进 `generated.rs` 生成管线（`scripts/refresh-model-catalog.sh` 同步上游 `generate-models.ts` 的新处理逻辑：Baseten 的 thinkingFormat 选择、deprecated 过滤、白名单 `assertExactModelIds` 等价校验）。

---

## 3. pir-agent 设计（R4.1–R4.2）

### 3.1 Agent 循环微行为（`agent.rs` / `agent_loop.rs`）

- `AgentOptions` 新增 `should_stop_after_turn: Option<Box<dyn Fn(&Agent, CancellationToken) -> bool>>`：turn 结束、队列轮询前调用。
- `BeforeToolCallResult` 新增 `terminate: bool`：与 `block: true` 组合；agent loop 在整批工具结果收集后判定"全部 terminate"→ 跳过后续模型调用，直接结束回合。
- `Agent::reset()`：活跃 run 期间返回错误（原来静默清状态）。
- `proxy.rs`：`toolcall_end` 事件帧携带完整 `ToolCall`（含 `namespace`），partial 合并改 `Object.assign` 等价语义。**此帧变化需同步 RPC/JSON 事件序列化与 fixtures**。

### 3.2 Compaction 契约（`compaction.rs`）

- `CompactionResult` → `CompactResult`：删 `first_kept_entry_id`，`retained_tail` 改必填，`extract_file_operations` 去掉 `from_hook` 检查，cut-point 只认 `branch_summary`。

### 3.3 Harness 层：明确不动

- `harness/` 保持 v1 语义。v4 lane 存储（R4.3 [DEFER]）不实现；仅在 `FileSystem` 等价抽象上**预留** `rename_file()`（原子发布是独立收益，且上游已将其设为必选——提前加可避免未来破坏性改 trait）。
- 在 `harness.rs` 模块文档中标注：上游 v2 为 scaffold，对齐待上游 H0+ 落地（引用 `external/pi/packages/agent/docs/harness-v2.md` §20）。

---

## 4. pir-tui 设计（R5.1–R5.5，本版本最大工程）

### 4.1 渲染器 trait 化重构

现有 `tui.rs` 单一大 struct 拆为：

```
crates/pir-tui/src/
├── tui.rs              # Tui trait（type-level 接口面）+ TuiMode + TuiStopOptions
├── tui_base.rs         # TuiBase：输入分发、overlay 栈、渲染调度、颜色查询（共有逻辑）
├── tui_main_screen.rs  # TuiMainScreen：现有差分渲染整体迁入（行为冻结，逐行等价）
├── tui_alt_screen.rs   # TuiAltScreen：全屏渲染器（新）
├── layout.rs           # LayoutBox/LayoutFrame/render_layout_frame + 滚动条几何
├── layout_node.rs      # LayoutNode trait（替代上游 LAYOUT_NODE symbol 协议）
└── components/{stack,v_stack,h_stack,scroll_view,alt_screen_flash}.rs
```

- `Tui` 在 Rust 用 trait object（`Box<dyn Tui>`）或泛型；`TuiBase` 以**基类复用**而非 trait 默认方法实现（字段共享：overlay 栈、渲染状态），与上游抽象基类同构。
- 运行时切换：`capture_render_state()/restore_render_state()`（main-screen 7 个状态字段）+ `TuiStopOptions { preserve_screen }`；`ViewportTui` trait（`set_layout_root`）。
- 上游已验证 `TuiMainScreen.doRender` 与旧 `TUI.doRender` 逐行等价——pir 迁移时**不允许**顺手改渲染逻辑，迁移后先跑旧黄金文件（重录基线前）确认等价。

### 4.2 布局引擎

- `LayoutNode` trait：任意组件实现即可参与布局（上游 symbol 协议的 Rust 对应）。
- `allocate_stack_sizes()`：basis/grow/shrink/min/max 约束求解，直接移植上游算法（含嵌套最小尺寸修复 `f24ab6e14`）。
- `ScrollView`：follow-end、overscroll 链式（`scroll_by` 返回未消费增量）、滚动条 `hidden|auto|always`、transient 1s 隐藏。
- render cache 按宽度键控；clip 沿帧树传播。

### 4.3 全屏渲染器

- 终端控制序列、鼠标解析（SGR/X10）、多路复用器 button-motion 降级、OSC 52 复制、OSC 133 prompt 导航、多击选择状态机（`DOUBLE_CLICK_INTERVAL_MS=500`、grapheme 边界吸附、边缘自动滚动 50ms）——按上游 `tui-alt-screen.ts`（1047 行）逐块移植。
- Kitty 图片：全局元数据注册表（LRU 1000 条）、placement-only 重发、像素级裁剪、离屏缓存（16 张/32MB/64MB 上限）。pir-tui 已有 `terminal_image.rs`，新增 `kitty_registry` 子模块。
- 退出重打：逐行 `\r\x1b[2K` 写主屏（剥 OSC 133 前缀）。
- 验收蓝本：`tui-alt-screen.test.ts` 30+ 场景 + `VirtualTerminal`/`RecordingTerminal` 等价的 pir-test-support VT 助手。

### 4.4 宽度算法与既有行为修正（`utils.rs`）

- `grapheme_width()` 按 R5.3.2 例外表重写：Unicode 数据生成脚本 `scripts/gen-tui-unicode-data.py` 同步更新（Spacing_Mark 例外、12 个非间距字符、Indic/FF00-FFEF/泰老 AM 尾随 +1 规则）。
- `truncate_to_width()` 插入 OSC 8 关闭序列（活跃超链接检测）；纯文本快路径跳过扫描。
- 颜色方案批量解析 regex、OSC 9;4 清除序列、SettingsList 空格语义、Editor 默认键位与新增 action——点状修正，各配回归。
- 输入即时渲染：`request_immediate_render()`（取消排队 timer，下一 tick 渲染）替代输入路径上的 16ms 节流；`render_now(force)` 同步渲染 API。**黄金文件基线在此步统一重录**。

### 4.5 LaTeX（`latex.rs`，新）

- tokenizer：`$...$`/`$$...$$`/`\(...\)`/`\[...\]`，含转义与 pending 未闭合状态（流式渲染友好）。
- 渲染：符号表、上下标、分数（display 模式垂直堆叠）、根式、矩阵/对齐/cases、limits、间距命令、重音符；内部 PUA 标记（`\u{f0000}`-`\u{f0005}`）布局后清除——直接移植上游 `latex.ts` 的对照表驱动设计。
- `Markdown` 组件新增 `render_latex`（默认开）与 `transform` 选项。

---

## 5. pir 主路径设计（R3.1–R3.7）

### 5.1 JSON/RPC 线格式（`modes/json_event.rs`，新）

```rust
// modes/json_event.rs：print-mode 与 rpc-mode 共用的唯一转换点
pub fn to_json_event(ev: &AgentSessionEvent) -> Option<JsonAgentSessionEvent>
```

- `message_update` 只发 delta（`content_index` + `delta`），删除累积 `message`/`partial` 字段；`message_end.message` 为权威终态。
- backpressure：stdout 写出封装 `wait_for_raw_stdout_backpressure()` 等价物（`tokio::io::Stdout` 可写等待 / 同步写时的缓冲水位检查），在事件订阅写出路径统一调用。
- `RpcClient` 事件类型同步 delta 形态；fixtures 与 `fixtures/generated/` 黄金 JSONL 由 `fixtures/generate-fixtures.mjs` 对新版上游重新生成。
- **这是全版本第一个交付项**：不落地则所有 JSON/RPC 集成对拍全红。

### 5.2 UI 模式接线

- CLI：`--tui-mode regular|fullscreen`（`--alt` 保留映射、帮助移除）；settings 键 `tuiMode`（旧 `uiMode` 忽略回退默认——与上游一致，不做迁移）。
- `interactive-mode` 等价模块新增 `switch_tui_mode()`：停旧渲染器（`preserve_screen`）→ capture/restore → 挂新渲染器，组件树重挂载。
- 设置项：`fullscreenExitOutput`、`fullscreenScrollbar`、主题色 `scrollbarThumb`。

### 5.3 会话行为簇（`core/`）

按依赖顺序实现（一个 PR 一串，不拆散）：

1. **length-stop 恢复链**（R3.4.1 + R3.4.2 四个上游 commit 整体移植）：`is_recoverable_length()` 判定 → 自动 compaction → 单次重试；`_overflow_recovery_attempted` 状态机修正；compaction 期间 `prompt()` 返回错误；`compaction_end` 前先清 controller。
2. **settings 深合并**（R3.4.3）：`deep_merge_settings` 改递归（对象递归、其余覆盖），配 #7572 场景测试。
3. **图片规范化**（R3.4.4）：`normalize_tool_result_images()` 挂 `after_tool_call`，在扩展 `tool_result` hook 之后执行。
4. **model 解析与刷新**（R3.4.5/R3.4.6）：精确 ID 歧义三态；可用性刷新代际计数（`availability_refresh_seq` 等）。
5. **凭证串行化**（R3.4.7）：`credential_operations` 串行 map + `CredentialSynchronizationError`。
6. **teardown 先 abort**（R3.4.9）、事件总线退订（R3.4.10）、find 相对化（R3.4.11）、`fetch_with_retry`（R3.4.12）。

### 5.4 auth 命令（`cli/auth_command.rs` 等）

- `pir auth print-api-key` / `print-bearer-token`（`--min-expiry`，默认 5 分钟阈值）、`pir auth check`（退出码 0/1/2）——薄 CLI 层，逻辑复用 pir-ai 的 R2.6 能力。

### 5.5 资源与包管理

- `resource-loader`：`AGENTS.override.md` 插入候选链首位；reload 保留 package source 元数据；`find_shadowed_context_file()` 用 git commonDir/mainRepoRoot 判影子。
- `package-manager`：git 安装容错（clean 失败检测依赖缺失重装、失败清理、`.pir-update-incomplete` marker）；`read_pi_manifest()` 独立化。

### 5.6 Mermaid 决策（R3.3.1）

上游用 grok-mermaid（TS）。**grok-mermaid 本身是 Rust 移植品**：源头是 [xai-org/grok-build](https://github.com/xai-org/grok-build)（Apache-2.0）的 `crates/codegen/xai-grok-markdown/src/mermaid.rs`——单文件 5237 行的自包含终端 Mermaid 渲染器（graph/flowchart、sequenceDiagram、stateDiagram → Unicode box-drawing，不支持的图类型回退带框原文）。因此 pir 不需要自绘，也不需要 JS 引擎：

- **方案：移植 Rust 原作**。以 `mermaid.rs` 为蓝本移植到 `pir-tui/src/mermaid.rs`（或独立子模块）。原作的对外接触面很小：`render(src, styles) -> Option<MermaidArt>`（行 + span 结构），唯二需要适配的是 `ratatui::style::Style`/`Line`/`Span` 与 `unicode-width`——前者映射到 pir-tui 自有样式模型，后者 pir-tui 已有等价物。
- **对拍基线**：pi 侧渲染结果即 grok-mermaid 输出，与 Rust 原作同算法；移植后可用上游 `mermaid.ts` 组件的测试用例 + grok-mermaid 的 fixtures 双向校验。
- **署名**：Apache-2.0，保留源文件头部出处声明与 LICENSE 归因（NOTICE 或文件头注释）。
- 未发布到 crates.io，采用移植而非依赖；上游 grok-build 持续同步 xAI monorepo，移植后把源文件 commit 哈希记入代码注释便于日后追更新。

工作量预估低于自绘子集，且天然与上游行为一致（同一算法源头）。

---

## 6. 扩展面设计（pir-ext-sdk / pir-ext-host，需求 §6）

- SDK：`scoped_models` 只读快照、`tool_call` 返回 `terminate`、`register_markdown_transformer`、`model_registry.complete/find/has_configured_auth`、async `set_runtime_api_key`、`get_api_key_and_headers` 返回 `Option<String>` 值（null 删除标记原样透传）。
- `refresh_models` context 重构直接影响 **Wasm ABI**：`stored` 快照 + `publish` 事务替代 `store` 读写——ABI v1 需加版本化新 host function 集（旧函数保留一个周期并标记 deprecated），更新 `docs/extension-abi.md` 与 ADR-0007 的缺口清单。
- 工具 system prompt 贡献常量外露到 SDK。
- TUI 类型面（`TuiMainScreen` 等）随 pir-tui trait 化同步进 SDK 的 UI 方法面。

---

## 7. 里程碑划分（建议）

| 里程碑 | 内容 | 出口标准 |
|--------|------|----------|
| M1 协议对齐 | R2.1 类型扩展、R3.1 JSON/RPC delta + backpressure、R4.1.4 proxy 帧 | JSON/RPC 对拍恢复绿；fixtures 重新生成 |
| M2 行为修正 | R2.3/R2.4 流式修复、R3.4 会话行为簇、R2.5/R2.6 models/OAuth、R3.5 资源 | 各 golden/回归通过；#7572/#7290/#7022 场景对拍 |
| M3 产品能力 | auth 命令族、R3.6/R3.7、新 provider、扩展 API 面（§6） | `auth check` 退出码对拍；ABI 文档更新 |
| M4 TUI 重构 | R5.1 trait 化 + R5.3 行为修正（基线重录） | main-screen 黄金文件等价 + 新基线入库 |
| M5 全屏子系统 | R5.2 全屏渲染器 + 布局引擎 + R5.4 LaTeX + R3.2 接线 | alt-screen 30+ 场景验收；模式热切换无重放 |

明确不做（[DEFER]/[VARIANT] 汇总）：server/protocol/client 远程栈、sqlite v4 后端、harness v2 运行时、telemetry 管线、deferred 请求生命周期、evals 体系。

---

## 8. 风险登记

1. **渲染基线重录**（M4 前置）：重录窗口内 TUI 测试不可作为回归门禁，需人工评审首批新基线。
2. **Wasm ABI 版本化**（§6）：`refresh_models` 是 ABI 级变更，处理不当会破坏已分发的 wasm 扩展包；版本化方案需先于 M3 评审。
3. **length-stop 恢复链**（M2）：四个上游 commit 相互依赖，移植时必须整体到位并配并发竞态测试，半成品比不做更危险。
4. **上游仍在动**：`4181f66` 之后 harness v2 的 D0/H0 落地时会再次冲击 session/record 契约；v0.11 的 defer 决策（§3.3）在那时需要重新评审。
5. **Unicode 宽度例外表**（§4.4）：`gen-tui-unicode-data.py` 的生成结果需与上游 `graphemeWidth` 全表对拍（码点级 diff），不能抽样。
