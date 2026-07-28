# T15：扩展宿主 L0+L1 与 Parity Freeze

- **状态**：未开始
- **里程碑**：M8
- **依赖**：T02（Wasm ABI spike）、T10（RPC UI 桥）、T12（Interactive UI 桥）
- **上游对照**：`docs/extensions.md`、`packages/coding-agent/src/core/extensions/*`
- **需求章节**：§9（L0+L1）、§11（parity freeze 对拍清单）、§1.2（成功标准总核对）
- **预估**：1.5–3 人月

---

## 目标

交付 Rust/Wasm 扩展宿主（API 形状同构，ADR-0001）与扩展安装管理，
并完成 Parity Freeze：全量对拍清单核对，宣布行为 parity（扩展语言除外）。

## 范围

### In

- `ExtensionHost` / `ExtensionApi` trait 定稿（`pir` crate，设计文档 §7.1）；能力面与 Pi ExtensionAPI 同构
- `pir-ext-host`：
  - `NativeExtensionHost`（L0）：内置扩展（Rust 编写）+ 动态库插件（`abi_stable`，已钉死）
  - `WasmExtensionHost`（L1）：wasmtime 宿主 + host ABI，能力面与 L0 对齐；runtime 嵌入主二进制
- 事件点接线：`project_trust` / `resources_discover` / `session_*` / `agent_*` / `tool_*` / provider hooks / `input` / `user_bash` 等；block/transform 结果合并
- 注册能力：`registerTool` / `Command` / `Shortcut` / `Flag` / `Provider`；`registerMessageRenderer` / `EntryRenderer`；`sendMessage` / `appendEntry` / 动态工具 / `setActiveTools`；扩展工具同名覆盖内置
- UI 桥三实现：`InteractiveUiBridge`（TUI 全能力）/ `RpcUiBridge`（对话框协议往返）/ `NullUiBridge`（print/json no-op）；RPC `ui.custom()` 不可用语义
- 扩展安装管理：本地路径 + 可分发 Wasm 包格式；`install` / `remove` / `list` / `update` / `config`；落盘 `~/.pir/agent/` 与 `.pir/`；启用/禁用与发现规则；`/reload`
- 沙箱：capability 授予，无默认全量文件/网络权限（编码规范 §11.4）
- 示例扩展（permission gate 等）+ ABI 文档 + 扩展脚手架
- Parity Freeze：全文档对拍清单（协议 / session 格式 / 扩展 API / TUI 行为四类）、session 互通终验、需求 §1.2 成功标准总核对

### Out

- TS 扩展兼容（永久非目标，ADR-0001）

## 开发要点

- 事件语义与 `docs/extensions.md` 逐条核对；emit 时机依赖 T05/T07/T10/T12 的事件点，缺事件点先补再登记偏离
- Wasm ABI 设计沿用 T02 spike 结论；ABI 文档与脚手架同步交付（生态冷启动，可行性 R1）
- 扩展安装复用 T14 packages 机制的发现/启用/禁用语义，Wasm 包 manifest 字段设计文档中未定的部分需补 ADR 或登记偏离
- parity freeze 清单建议落成 `docs/parity-checklist.md`，逐项标注对拍证据

## 进度跟踪

- [ ] 设计细化
- [ ] 实现
- [ ] 自测
- [ ] 门禁验收
- [ ] 文档回写

## 自测清单

- [ ] L0 内置扩展 e2e：注册工具 → agent 调用 → block/transform 生效
- [ ] L1 Wasm 扩展 e2e：同一能力面（示例扩展 Rust 与 Wasm 双实现行为一致）
- [ ] 事件点逐条触发测试（对照 `docs/extensions.md` 清单）
- [ ] UI 桥：Interactive dialog VT 测试、RPC 对话框协议往返契约测试、print/json no-op 断言
- [ ] 扩展工具同名覆盖内置工具语义正确
- [ ] 安装管理 e2e：本地路径与 Wasm 包的 install/list/config（禁用/启用）/remove
- [ ] 沙箱：未授权 capability 的 Wasm 调用被拒绝
- [ ] `/reload` 热加载语义

## 门禁验收

通用门禁 G1–G7 全过。

任务特有标准：

- [ ] 需求 §9 能力清单逐条核对有锚点（验收记录列映射表）
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
