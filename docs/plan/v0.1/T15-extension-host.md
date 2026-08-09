# T15：扩展宿主 L0+L1 与 Parity Freeze

- **状态**：已完成
- **里程碑**：M8
- **依赖**：T02（Wasm ABI spike）、T10（RPC UI 桥）、T12（Interactive UI 桥）
- **上游对照**：`docs/extensions.md`（2961 行，全量规格）、`packages/coding-agent/src/core/extensions/*`、`docs/custom-provider.md`
- **需求章节**：§9（L0+L1）、§11（parity freeze 对拍清单）、§1.2（成功标准总核对）
- **预估**：2–3 人月

---

## 目标

交付 Rust/Wasm 扩展宿主（API 形状同构，ADR-0001）与扩展安装管理，
并完成 Parity Freeze：全量对拍清单核对，宣布行为 parity（扩展语言除外）。

## 范围

### In

- `ExtensionHost` / `ExtensionApi` trait 定稿（`rpi` crate，设计文档 §7.1）；**能力面与 Pi ExtensionAPI 同构：33 事件 + 24 API 方法 + 28 UI 方法 + 三级 Context**（需求 §9.1 全集）
- `rpi-ext-host`：
  - `NativeExtensionHost`（L0）：内置扩展（Rust 编写，含 llama.cpp 隐藏扩展的宿主化）+ 动态库插件（`abi_stable`，已钉死）
  - `WasmExtensionHost`（L1）：wasmtime 宿主 + host ABI，能力面与 L0 对齐；runtime 嵌入主二进制
- **事件全集接线（33 个）**：`project_trust` / `resources_discover`（可补充资源路径）/ `session_start` / `session_info_changed` / `session_before_switch` / `session_before_fork` / `session_before_compact` / `session_compact` / `session_shutdown` / `session_before_tree` / `session_tree` / `context` / `before_provider_request` / `before_provider_headers` / `after_provider_response` / `before_agent_start` / `agent_start` / `agent_end` / `agent_settled` / `turn_start` / `turn_end` / `message_start` / `message_update` / `message_end` / `tool_execution_start` / `tool_execution_update` / `tool_execution_end` / `model_select` / `thinking_level_select` / `user_bash` / `input` / `tool_call` / `tool_result`；**可变语义**（tool_call 原地改参/block、tool_result 改结果、input 三态、user_bash 换 operations、before_agent_start 注入+链式替换、before_provider_request handler 返回值整体替换 payload（链式，undefined 不替换，runner.ts:1003-1035）、before_provider_headers 原地 mutate headers（返回值忽略，值设 null 删除 header，runner.ts:1037-1063）、session_before_* cancel、message_end 替换保 role、context 替换 messages）
- **API 方法全集（24 个，含 `events` 属性）**：事件 on() + `registerTool` / `registerCommand` / `registerShortcut`（restrictOverride）/ `registerFlag`+`getFlag` / `registerMessageRenderer` / `registerEntryRenderer` / `sendMessage`（deliverAs/triggerTurn）/ `sendUserMessage` / `appendEntry` / `setSessionName`/`getSessionName` / `setLabel` / `exec` / `getActiveTools`/`getAllTools`/`setActiveTools` / `getCommands` / `setModel` / `getThinkingLevel`/`setThinkingLevel` / `registerProvider`（双签名）/ `unregisterProvider` / `events` EventBus
- **UI 方法全集（28 个）**：需求 §9.1 清单；组件工厂类（setWidget/setFooter/setHeader/custom/setEditorComponent）采用**声明式组件描述 + 协议往返**（M0 spike 结论落地；序列化格式本任务定稿并回写设计文档 §13）
- **Context 三级**：ExtensionContext（isIdle/abort/shutdown/getContextUsage/compact/getSystemPrompt 等）+ CommandContext（newSession/fork/navigateTree/switchSession/reload/waitForIdle；session 替换后旧 ctx 作废）+ ReplacedSessionContext
- **动态工具**：工具执行期间 `setActiveTools` 新增经 `addedToolNames` 暴露
- UI 桥三实现：`InteractiveUiBridge`（TUI 全能力）/ `RpcUiBridge`（**9 方法协议往返 + 降级清单**：custom() 返回 undefined、theme 切换错误、getEditorText 恒 "" 等，需求 §2.4）/ `NullUiBridge`（print/json no-op）
- 加载与发现：`~/.rpi/agent/extensions` 与 `.rpi/extensions`（一层）、packages、CLI `-e`、inline factory；冲突诊断 + 加载顺序优先；**同名冲突规则**：工具/flag 首注册胜+有诊断；renderer 首注册胜且静默；command 全部保留、重名加 `:N` 后缀、扩展间重名无诊断；shortcut last-wins+诊断（runner.ts:446-629、resource-loader.ts:1013-1038）；`/reload`；模块缓存按 cwd+generation
- 扩展安装管理：本地路径 + 可分发 Wasm 包格式；`install` / `remove` / `list` / `update` / `config`；落盘 `~/.rpi/agent/` 与 `.rpi/`；启用/禁用与发现规则
- 沙箱：capability 授予，无默认全量文件/网络权限（编码规范 §11.4）
- 示例扩展（permission gate 等）+ ABI 文档 + 扩展脚手架
- Parity Freeze：全文档对拍清单（协议 / session 格式 / 扩展 API / TUI 行为四类）、session 互通终验、需求 §1.2 成功标准总核对

### Out

- TS 扩展兼容（永久非目标，ADR-0001）

## 开发要点

- 事件语义与 `docs/extensions.md` 逐条核对（含生命周期图各序列）；emit 时机依赖 T05/T07/T10/T12/T16 的事件点，缺事件点先补再登记偏离
- Wasm ABI 设计沿用 T02 spike 结论；ABI 文档与脚手架同步交付（生态冷启动，可行性 R1）
- 扩展安装复用 T14 packages 机制的发现/启用/禁用语义；Wasm 包 manifest 字段（设计文档 §13 开放项）本任务定稿并回写
- parity freeze 清单落成 `docs/parity-checklist.md`，逐项标注对拍证据

## 进度跟踪

- [x] 设计细化
- [x] 实现
- [x] 自测
- [x] 门禁验收
- [x] 文档回写

## 设计细化（2026-08-08）

勘察结论（三份简报）：`rpi` crate 已有 `ExtensionRunner` trait（`core/extensions.rs:252`，全默认 no-op + `NoopExtensionRunner` + `ExtensionRunnerRef` 可变槽位）；33 事件中 23 个发射点已就位，10 个缺口（tool_call、tool_result、session_before_compact、session_compact、after_provider_response、user_bash、project_trust、resources_discover，及 before_agent_start/before_provider_request 链式语义核对）；`AgentLoopConfig.before_tool_call/after_tool_call` 钩子已存在但 sdk.rs 未挂（harness 有现成实现可移植）；RPC 9 方法协议与 pending_ui 路由已在 rpc.rs 钉死；`extend_resources` 消费端已就绪无调用方。

### 波次拆分

- **W1 宿主核心（rpi-ext-host）**：`ExtensionFactory`/`Extension` 对象模型、`ExtensionApiImpl`（pi 对象）、`NativeExtensionHost`（实现 `ExtensionRunner` trait）；注册表与同名冲突规则全量（工具首注册胜/同扩展覆盖/覆盖内置+渲染 slot 继承、flag 首注册+诊断、command 全保留+`:N` 后缀、shortcut last-wins+保留键 restrictOverride、renderer 首注册静默）；发现与加载（`.rpi/extensions` + `~/.rpi/agent/extensions` 一层 + packages + CLI `-e` + inline；顺序优先；cwd+generation 缓存；错误隔离）；emit 串行分发 + ExtensionError 总线。单测锚定 runner.ts:446-629 / resource-loader.ts:1013-1038。
- **W2 事件缺口接线（rpi crate）**：sdk.rs 挂 before/after_tool_call 钩子（tool_call 改参/block/fail-safe、tool_result 局部补丁链式）；compaction_runner 接 session_before_compact（cancel/compaction 替换/fromExtension）与 session_compact；sdk.rs stream_fn 补 on_response → after_provider_response；interactive user_bash 拦截（operations/result 替换，首个非空胜）；app.rs 接通 emit_project_trust；bind_extensions 后 emit_resources_discover + extend_resources。逐事件测试。
- **W3 24 API 方法动作绑定**：bind_core 动作全集（sendMessage deliverAs/triggerTurn 三分支、sendUserMessage streaming 无 deliverAs 抛错、appendEntry、setSessionName/getSessionName、setLabel、exec、getActiveTools/getAllTools（含 sourceInfo）/setActiveTools（未注册静默忽略+重建 system prompt+active_tools_change 条目核对）、getCommands 顺序、setModel 无 key 返 false、thinking get/set+clamp、registerProvider 双签名+pending 队列冲刷、unregisterProvider 恢复内建、registerFlag/getFlag 与 unknown_flags 接线（替换 apply_extension_flag_values 报错路径）、events 共享总线）。
- **W4 28 UI 方法与三桥**：`UiBridge` trait（28 方法）；`NullUiBridge`（noOpUIContext 逐项语义）；`RpcUiBridge`（9 方法协议往返 + 18 降级逐项断言）；`InteractiveUiBridge`（复用 commands_selectors oneshot 对话框模式）；声明式组件描述 JSON → rpi-tui 组件渲染（widget/footer/header/custom overlay 往返）。RPC 契约测试 + VT 测试。
- **W5 三级 Context 与生命周期**：ExtensionContext（isIdle/isProjectTrusted/signal/abort/hasPendingMessages/shutdown 模式差异/getContextUsage/compact 不 await/getSystemPrompt）→ CommandContext（getSystemPromptOptions/waitForIdle/newSession/fork/navigateTree/switchSession/reload）→ ReplacedSessionContext（sendMessage/sendUserMessage 绑新 session）；stale 失效（invalidate/assertActive）；session 替换时序（旧 shutdown→teardown→重绑→新 session_start→withSession）。
- **W6 L1 Wasm 宿主**：ABI v1 定稿（见下）；`WasmExtensionHost`（wasmtime 47，每扩展独立 Store + 专属线程，host call 阻塞等 oneshot；事件分发串行）；capability 沙箱（manifest 声明、逐 host call 校验、无 WASI 默认权限）；guest SDK crate + 脚手架；示例扩展 Rust/Wasm 双实现行为一致对拍；ABI 文档。
- **W7 安装管理与内置宿主化**：Wasm 包 install/remove/list/update/config（复用 package_manager 发现/启用/禁用语义，补 app.rs packages→loader 运行时接线）；llama 迁移真扩展（关闭 D-047）；switchSession 异 cwd 信任选择器接线（关闭 ADR-0006/D-044）；D-045 渲染器缺口评估；share_viewer_base_url 死代码清理；--wasm-smoke 钩子去留。
- **W8 Parity Freeze 与门禁**：`docs/parity-checklist.md`（协议/session 格式/扩展 API/TUI 行为四类，逐项对拍证据）；session 互通终验；需求 §1.2 成功标准总核对；二进制体积复测 < 50MB；G1–G7；偏离登记回写；验收记录。

### 开放项定稿（回写设计 §13）

**1. Wasm ABI v1（字节布局）**——沿用 T02 spike 形状，收敛为两条泛化通道：
- guest 导出：`memory`、`pir_alloc(len:u32)->u32`、`pir_dealloc(ptr:u32,len:u32)`、`rpi_extension_init()->u64`、`rpi_dispatch(ptr:u32,len:u32)->u64`；
- host 导入（模块名 `rpi`）：`pir_host_call(ptr:u32,len:u32)->u64`；
- 载荷均为 guest 线性内存中的 UTF-8 JSON：guest→host 传 `(ptr,len)`；host→guest 返回 `u64 = (ptr<<32)|len`（由 `pir_alloc` 分配，guest 读后 `pir_dealloc`）；
- `pir_host_call` 请求 `{"call":"<method>","args":{...},"seq":N}`，响应 `{"ok":...}|{"error":{"kind","message"}}`；method 覆盖 24 API（`registerTool` 等）+ 28 UI（`ui.select` 等）+ events 总线；
- `rpi_dispatch` 消息 `{"kind":"event","event":"<name>","payload":{...}}` → 返回 handler 结果 JSON；`{"kind":"toolExecute",...}` / `{"kind":"render",...}`；每扩展分发串行（镜像上游 handler 串行语义）；
- 并发模型：每扩展实例独立 Store + 专属阻塞线程，host call 内经 channel 等 async 侧 oneshot 结果。

**2. Wasm 包 manifest（`rpi-extension.json`，目录级）**：
```json
{
  "name": "my-ext",
  "version": "0.1.0",
  "description": "...",
  "wasm": "dist/my_ext.wasm",
  "capabilities": ["tools","commands","ui","session","exec","provider","events"],
  "rpiAbi": 1
}
```
发现规则与一层目录约定复用 `.rpi/extensions` 既有语义；裸 `.wasm` 文件按 capabilities=[]（仅 on 订阅）处理；capability 逐 host call 强制（拒绝返 `capabilityDenied`），无默认文件/网络权限（不链 WASI）。

**3. 声明式组件描述序列化格式**：JSON 组件树 `{"type":"box|text|spacer|row|column","props":{...},"children":[...]}`，props 含内容/样式（fg/bg/bold/padding/border 等）；host 侧映射 rpi-tui 组件渲染；用户交互经 `rpi_dispatch` `{"kind":"uiEvent","widgetKey","event":{...}}` 回传，guest 返回更新后组件树二次渲染（spike 已验证往返闭环）。

## 自测清单

- [x] L0 内置扩展 e2e：注册工具 → agent 调用 → block/transform 生效（`w6_native_and_wasm_gate_behave_identically`、w2 tool_call/tool_result 六例、`llama_extension.rs` 17 例 loopback）
- [x] L1 Wasm 扩展 e2e：同一能力面（`w6_native_and_wasm_gate_behave_identically` 同 gate 双实现行为一致；`wasm_test.rs` 7 例；`w6_wasm_tool_executes_and_gate_blocks_in_agent_loop`）
- [x] 33 事件逐条触发测试（`docs/parity-checklist.md` §3.1 逐条锚点表：触发点 + 分发点 + 测试名）
- [x] 事件可变语义各分支（block/改参/三态/cancel/替换：runner_test.rs `runner_tool_call_*`/`runner_input_*`/`runner_message_end_*`/`runner_session_before_*`/`runner_context_*`/`runner_before_provider_*`；w2 六例）
- [x] UI 桥：Interactive dialog VT 测试（w4 组件树/renderer VT 投影断言）、RPC 9 方法协议往返契约测试 + 降级清单逐项断言（`w4_rpc_*` 7 例）、print/json no-op 断言（`null_bridge_matches_noop_ui_context`）
- [x] 声明式组件描述：widget/footer/header/custom overlay 往返渲染（`w4_component_tree_text_spacer_box_column`、`w4_tool_render_override_and_inheritance`、`w4_message_renderer_descriptor_renders_in_tui`；RPC 臂 `w4_rpc_fire_and_forget_frames`/`w4_rpc_degraded_methods`）
- [x] 扩展工具同名覆盖内置工具语义正确（`w3_extension_tool_overrides_builtin_definition_and_execution`）；addedToolNames 动态工具（`w3_added_tool_names_pure_addition_branch`、`w3_added_tool_names_suppressed_when_tools_removed`）
- [x] 安装管理 e2e：本地路径与 Wasm 包的 install/list/config（禁用/启用）/remove（`w7_install_list_disable_enable_remove_wasm_package`、`w7_installed_wasm_package_loads_and_blocks`）
- [x] 沙箱：未授权 capability 的 Wasm 调用被拒绝（`wasm_capability_denied_for_bare_guest_and_allowed_by_manifest`、`native_plugin_capability_denied_without_tools`）
- [x] `/reload` 热加载语义（`w5_reload_reruns_factories_preserves_flags_and_stales_old`、`w5_session_reload_event_sequence`）

## 门禁验收

通用门禁 G1–G7 全过。

任务特有标准：

- [x] 需求 §9 能力清单（33+24+28+3）逐条核对有锚点（`docs/parity-checklist.md` §3，88 条映射表）
- [x] parity checklist 全项通过或有 ADR 钉死的有意差异（`docs/parity-checklist.md` §5：ADR-0001~0007；ADR-0007 三缺口钉死）
- [x] session 互通终验：Pi fixtures 加载续跑（`parity_fixture_session_prompt_continue_with_faux_provider`，W8 补全栈 faux prompt 续跑）+ rpi 产出被上游格式校验通过（口径：归一化 diff 反向表达，见 checklist §2 末行）
- [x] ABI 文档与示例扩展随代码交付（`docs/extension-abi.md` v1 + §1.1 L0 段；`examples/wasm-extension/`；`crates/rpi-test-native-plugin`）
- [x] 二进制体积复测仍 < 50MB（需求 §11.2）：gnu release 实测 32,125,208 字节 ≈ 30.6 MiB（2026-08-09）

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| D-048 | 扩展宿主核心动作与事件落地差异（tool_call 改参穿线 / user_bash operations 丢弃回退 / ProviderConfig 闭包拒绝 / newSession setup 省略 / exec SIGKILL / 非 RPC 模式 command.* 未绑；第 2、6 条行为级经 ADR-0007） | 已回写 |
| D-049 | 扩展 UI/渲染层差异（custom() 声明式 v1 无交互回传——ADR-0007；ComponentTree schema v1 无 row） | 已回写 |
| D-050 | L0 原生动态库插件（abi_stable）落地差异（RpiHostCalls 按值 / cookie / RVec；无沙箱信任模型；manifest `native` 字段） | 已回写 |
| D-044 | switchSession 异cwd 信任提示降级（ADR-0006）——W7 已接线异步信任选择器，偏离消除 | 已关闭 |
| D-047 | llama 直供表临时机制——W7 已迁移为经真宿主加载的内置 hidden 扩展，临时机制移除 | 已关闭 |
| D-045 | renderedTools 不移植——W7 复核维持登记（扩展渲染树为 JSON 描述符，无 ANSI 形态可喂 export 管线），结论见 D-045「T15 W7 复核结论」 | 已关闭 |

## 验收记录

- 验收日期：2026-08-09
- 验收人：单人开发按清单逐项自证（Kimi Code CLI 子代理执行，命令输出实跑摘录）
- G1 构建/静态检查：通过。`cargo build --workspace` / `cargo clippy --workspace --all-targets -- -D warnings` / `cargo fmt --all -- --check` 全绿（W8 修复 W7 遗留的 4 处 --all-targets 级问题：type_complexity 两处抽别名、5 处测试初始化缺 `select_async` 字段、cloned_ref_to_slice 与 useless_conversion 各一）
- G2 测试：通过。**3815 passed, 0 failed**（`cargo test --workspace`，W8 净增 2 例：`parity_fixture_session_prompt_continue_with_faux_provider`、`runner_entry_renderer_first_registration_wins_silently`；另扩展 `api_actions_forward_after_bind` 覆盖 appendEntry/setLabel 转发）；无 live 测试参与（未设 `RPI_LIVE_TEST=1`）；非 live 测试全走 faux provider / loopback
- G3 对拍：通过。session JSONL（`parity_session_test.rs` 3 例 + `parity_compaction_test.rs` + `parity_harness_interop_test.rs`）、事件流（`parity_headless_test.rs` 5 场景）、RPC（`rpc_mode_test.rs` 36 例）、compaction golden、资源 golden（`parity_resources_test.rs`）、TUI 快照黄金 36 个；逐条对拍级基准的「文档条目 → 测试锚点」映射见 `docs/parity-checklist.md`（session-format.md → §2、rpc.md → §1.2、compaction.md → §2、keybindings.md/tmux.md/terminal-setup.md → §4、extensions.md → §3）
- G4 红线：通过。`external/pi` `git status --porcelain` 为空且 HEAD=`2efa728d2ee90ef597626e96b1e28ef2b279f07c`（git 实测）；无 JS/TS 执行能力（无 deno/node/quickjs 依赖）；未读写 `~/.pi`/`.pi`（全库 grep 仅 test 内上游格式字符串）；session 仅 JSONL（无 SQLite）；token 估算未动（T03/T08 钉死版）；非测试代码无 unwrap/expect 违规新增（rpi-ext-host/rpi-ext-sdk/扩展相关文件抽查：仅不变式注释 `expect` 与测试代码）；日志/错误无凭据（D-005/D-047 口径维持）；范围排除项未引入；grep/find/ls 无外部二进制下载（T14 原生实现）；session 写入无文件锁（session_manager.rs / rpi-agent session 无 flock）
- G5 线格式：通过。`rpi-extension.json` manifest（`WasmManifest` serde camelCase：`rpiAbi`/`capabilities`/`wasm`/`native`）、ABI JSON method 表（camelCase）、RPC/UI 帧（`extension_ui_request` 等）抽查与上游形状一致；fixtures 对拍见 G3
- G6 文档同步：通过。移植代码溯源注释维持（上游路径 + 行号）；偏离回写——`02-design.md` §7.2（L0 落地注记）/§13（ABI/manifest/组件 schema 三开放项定稿）、`docs/extension-abi.md`（§1.1 L0 段 + §5 native 字段）；交付物描述同步——`README.md` 状态节（M0 骨架 → v0.1 全任务完成）+ 文档表加 ABI/parity-checklist 两行；`coding-standards.md`/`UPSTREAM.md` 复查无过时；`rpi-ext-host/src/lib.rs` 头注核对（W6 已更新，无需再改）
- G7 偏离闭环：通过。D-048/D-049/D-050 新建并回写（D-048 第 2/6 条、D-049 第 1 条行为级 → ADR-0007）；D-044（ADR-0006）/D-047 关闭；D-045 复核维持关闭；deviations/README.md 登记表与本任务偏离记录表同步
- 任务特有标准：① 需求 §9 能力清单 88 条逐条锚点（parity-checklist §3）✅；② parity checklist 全项通过或 ADR 钉死（§5，ADR-0001~0007）✅；③ session 互通终验 ✅（W8 补全栈 faux 续跑；rpi 产出校验口径 = 归一化 diff 反向表达，checklist §2 注明）；④ ABI 文档 + 示例扩展交付 ✅；⑤ 二进制体积 gnu release 实测 **32,125,208 字节 ≈ 30.6 MiB < 50MB** ✅（需求 §11.2；T14 口径沿用，较 T14 的 29MB 增 ~3MB 为 wasmtime 真宿主 + abi_stable 落地）
- 需求 §1.2 成功标准总核对：五条全过（逐条证据见 `docs/parity-checklist.md` §6）
- 结论：**通过**
