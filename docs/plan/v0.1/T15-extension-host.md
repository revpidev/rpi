# T15：扩展宿主 L0+L1 与 Parity Freeze

- **状态**：未开始
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

- `ExtensionHost` / `ExtensionApi` trait 定稿（`pir` crate，设计文档 §7.1）；**能力面与 Pi ExtensionAPI 同构：33 事件 + 24 API 方法 + 28 UI 方法 + 三级 Context**（需求 §9.1 全集）
- `pir-ext-host`：
  - `NativeExtensionHost`（L0）：内置扩展（Rust 编写，含 llama.cpp 隐藏扩展的宿主化）+ 动态库插件（`abi_stable`，已钉死）
  - `WasmExtensionHost`（L1）：wasmtime 宿主 + host ABI，能力面与 L0 对齐；runtime 嵌入主二进制
- **事件全集接线（33 个）**：`project_trust` / `resources_discover`（可补充资源路径）/ `session_start` / `session_info_changed` / `session_before_switch` / `session_before_fork` / `session_before_compact` / `session_compact` / `session_shutdown` / `session_before_tree` / `session_tree` / `context` / `before_provider_request` / `before_provider_headers` / `after_provider_response` / `before_agent_start` / `agent_start` / `agent_end` / `agent_settled` / `turn_start` / `turn_end` / `message_start` / `message_update` / `message_end` / `tool_execution_start` / `tool_execution_update` / `tool_execution_end` / `model_select` / `thinking_level_select` / `user_bash` / `input` / `tool_call` / `tool_result`；**可变语义**（tool_call 原地改参/block、tool_result 改结果、input 三态、user_bash 换 operations、before_agent_start 注入+链式替换、before_provider_request handler 返回值整体替换 payload（链式，undefined 不替换，runner.ts:1003-1035）、before_provider_headers 原地 mutate headers（返回值忽略，值设 null 删除 header，runner.ts:1037-1063）、session_before_* cancel、message_end 替换保 role、context 替换 messages）
- **API 方法全集（24 个，含 `events` 属性）**：事件 on() + `registerTool` / `registerCommand` / `registerShortcut`（restrictOverride）/ `registerFlag`+`getFlag` / `registerMessageRenderer` / `registerEntryRenderer` / `sendMessage`（deliverAs/triggerTurn）/ `sendUserMessage` / `appendEntry` / `setSessionName`/`getSessionName` / `setLabel` / `exec` / `getActiveTools`/`getAllTools`/`setActiveTools` / `getCommands` / `setModel` / `getThinkingLevel`/`setThinkingLevel` / `registerProvider`（双签名）/ `unregisterProvider` / `events` EventBus
- **UI 方法全集（28 个）**：需求 §9.1 清单；组件工厂类（setWidget/setFooter/setHeader/custom/setEditorComponent）采用**声明式组件描述 + 协议往返**（M0 spike 结论落地；序列化格式本任务定稿并回写设计文档 §13）
- **Context 三级**：ExtensionContext（isIdle/abort/shutdown/getContextUsage/compact/getSystemPrompt 等）+ CommandContext（newSession/fork/navigateTree/switchSession/reload/waitForIdle；session 替换后旧 ctx 作废）+ ReplacedSessionContext
- **动态工具**：工具执行期间 `setActiveTools` 新增经 `addedToolNames` 暴露
- UI 桥三实现：`InteractiveUiBridge`（TUI 全能力）/ `RpcUiBridge`（**9 方法协议往返 + 降级清单**：custom() 返回 undefined、theme 切换错误、getEditorText 恒 "" 等，需求 §2.4）/ `NullUiBridge`（print/json no-op）
- 加载与发现：`~/.pir/agent/extensions` 与 `.pir/extensions`（一层）、packages、CLI `-e`、inline factory；冲突诊断 + 加载顺序优先；**同名冲突规则**：工具/flag 首注册胜+有诊断；renderer 首注册胜且静默；command 全部保留、重名加 `:N` 后缀、扩展间重名无诊断；shortcut last-wins+诊断（runner.ts:446-629、resource-loader.ts:1013-1038）；`/reload`；模块缓存按 cwd+generation
- 扩展安装管理：本地路径 + 可分发 Wasm 包格式；`install` / `remove` / `list` / `update` / `config`；落盘 `~/.pir/agent/` 与 `.pir/`；启用/禁用与发现规则
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

- [ ] 设计细化
- [ ] 实现
- [ ] 自测
- [ ] 门禁验收
- [ ] 文档回写

## 自测清单

- [ ] L0 内置扩展 e2e：注册工具 → agent 调用 → block/transform 生效
- [ ] L1 Wasm 扩展 e2e：同一能力面（示例扩展 Rust 与 Wasm 双实现行为一致）
- [ ] 33 事件逐条触发测试（对照 `docs/extensions.md` 生命周期图）
- [ ] 事件可变语义各分支（block/改参/三态/cancel/替换）
- [ ] UI 桥：Interactive dialog VT 测试、RPC 9 方法协议往返契约测试 + 降级清单逐项断言、print/json no-op 断言
- [ ] 声明式组件描述：widget/footer/header/custom overlay 往返渲染
- [ ] 扩展工具同名覆盖内置工具语义正确；addedToolNames 动态工具
- [ ] 安装管理 e2e：本地路径与 Wasm 包的 install/list/config（禁用/启用）/remove
- [ ] 沙箱：未授权 capability 的 Wasm 调用被拒绝
- [ ] `/reload` 热加载语义

## 门禁验收

通用门禁 G1–G7 全过。

任务特有标准：

- [ ] 需求 §9 能力清单（33+24+28+3）逐条核对有锚点（验收记录列映射表）
- [ ] parity checklist 全项通过或有 ADR 钉死的有意差异
- [ ] session 互通终验：Pi fixtures 加载续跑 + pir 产出被上游格式校验通过
- [ ] ABI 文档与示例扩展随代码交付
- [ ] 二进制体积复测仍 < 50MB（需求 §11.2）

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| — | （暂无） | — |

## 验收记录

（待填写，模板见 `gates.md` §3）
