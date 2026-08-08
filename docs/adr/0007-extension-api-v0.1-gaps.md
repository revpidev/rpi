# ADR-0007：v0.1 扩展 API 三处功能缺口（user_bash operations / 非 RPC 模式 command.* 未绑 / custom() 无交互回传）

- **状态**：已采纳
- **日期**：2026-08-08
- **关联**：[`01-requirements.md`](../01-requirements.md) §9、[`02-design.md`](../02-design.md) §7、
  T15 偏离 D-048（第 2、6 条）、D-049（第 1 条）

## 背景

T15 扩展宿主（L0 native + L1 wasm）落地后，扩展 API 面与上游
`packages/coding-agent/src/core/extensions/` @ 0.82.1（2efa728）逐条核对，发现三处
功能缺口，均属「闭包/交互通道无法跨 JSON 边界或尚未接线」一类：

1. **`user_bash` 的 `operations` 自定义执行后端不支持**：上游
   `UserBashEventResult.operations` 允许扩展返回一组文件操作闭包来替换内置 bash
   执行后端；闭包束不可序列化，pir 收到后丢弃并回退内置执行
   （`crates/pir-ext-host/src/types.rs:674` 注释、W2 测试
   `w2_user_bash_operations_only_and_no_handler_fall_back` 钉死）。
2. **非 RPC 模式 `ctx.command.*` 未绑定**：上游
   `runner.bindCommandContext` 在 `agent-session.ts:2309` 无条件接线（全模式真实现）；
   pir 仅在 RPC 模式绑定（`crates/pir/src/modes/rpc.rs:707`），interactive / print
   模式走未绑定默认值（对齐上游 `runner.ts:421-427` 的默认：
   `{cancelled: false}` / no-op）。即扩展命令在交互模式内调
   `ctx.newSession()/fork()/switchSession()/reload()` 不产生效果。
3. **`ctx.ui.custom()` 声明式 v1 无交互回传**：上游 `custom()` 挂载获得键盘焦点的
   交互组件并以 `done(result)` 值 resolve；pir 的声明式组件树（ComponentTree v1）
   没有交互通道，W4 实现为「展示后立即 resolve `undefined`」
   （`crates/pir/src/modes/interactive/interactive_mode/ui_bridge.rs:413` 注释）。

## 决策

v0.1 接受上述三处缺口（按行为级偏离登记于 D-048 / D-049）：

- 缺口 1、3 是架构性约束：扩展 API 的 wasm/JSON 边界（ADR-0002 决策的 L0+L1 后端、
  声明式组件协议）天然无法传闭包与有状态交互组件；要支持需引入回调句柄协议
  （host 侧注册回调、guest 侧持句柄触发），复杂度与 v0.1 范围不匹配。
- 缺口 2 是接线范围决策：T15 的 command actions 真实现
  （`RuntimeCommandActions`，`crates/pir/src/core/extension_context.rs`）依赖
  `AgentSessionRuntime` 持有权，RPC 模式已接线；interactive 模式的绑定需解决
  与 TUI 事件泵的重入时序，留待后续版本（不影响内置命令与 `/llama` 等已迁移
  扩展——它们不经 `ctx.command.*`）。

共同约束：三处缺口**不静默**——operations 丢弃有注释与测试钉死、command.* 返回
上游定义的默认值、custom() 立即 resolve，扩展作者可探测；宿主不产生半执行状态。

## 后果

- v0.1 发布说明需注明三处已知差异；`docs/extension-abi.md` 为扩展作者口径的
  权威文档，相关条目已在其 method 表注记。
- W8 parity freeze 将三处列入扩展 API 类的「有意差异」清单，不作为对拍失败项。
- 后续版本若引入回调句柄协议 / interactive command actions 绑定，须回写本 ADR
  并关闭 D-048 第 2、6 条与 D-049 第 1 条。
