# D-048：T15 扩展宿主核心动作与事件落地差异（汇总型）

- **状态**：已回写
- **关联任务**：T15（W1–W7）
- **级别**：第 2、6 条行为级（功能缺口，ADR-0007）；余为实现细节偏离
- **发现日期**：2026-08-08（W2–W7 各波次汇报候选的汇总复核登记）

## 原文档约定

- 上游基准：`packages/coding-agent/src/core/extensions/{runner,types,loader}.ts`、
  `agent-session.ts`（扩展动作绑定）@ 0.82.1（2efa728）。
- 需求 §9（扩展能力面：33 事件 + 24 API 方法 + 28 UI 方法 + 三级 Context）；
  设计 §7（扩展宿主设计）。

## 实际实现与偏离原因

1. **tool_call 跨 handler 改参经结果穿线**：上游把同一个可变 `event` 对象依次传给
   各 handler，改参靠原地改共享对象（runner.ts:919-940）；JSON/wasm 边界无共享
   引用，pir 改为 handler 结果 `input` 字段穿线——后续 handler 看到改后参数，
   最终结果合并改后 `input`（`crates/pir-ext-host/src/runner.rs:640-663`）。
   对规范使用（返回改后参数）的扩展行为等价；`block` 短路语义一致。
2. **（行为级，ADR-0007）`user_bash` 的 `operations` 不支持**：上游
   `UserBashEventResult.operations` 为自定义 bash 执行后端的闭包束，不可序列化；
   pir 丢弃该字段并回退内置执行（`crates/pir-ext-host/src/types.rs:674` 注释；
   测试 `w2_user_bash_operations_only_and_no_handler_fall_back`）。
3. **`registerProvider` 的闭包子项显式拒绝**：`ProviderConfig` 的
   `streamSimple` / `oauth` / `refreshModels` 为闭包字段，无法跨 JSON 边界；
   pir 大声报错而非静默丢弃（`crates/pir/src/core/extension_actions.rs:262-265`）。
   Rust 侧 provider 走 `register_native_provider`（持 trait 对象，无序列化问题）。
4. **`newSession` 的 `setup` 回调省略**：上游 setup 接收 `SessionManager` 实例，
   不可跨宿主边界表达；v1 以 `withSession`（接收替换后会话的扩展上下文）替代
   （`crates/pir-ext-host/src/api.rs:591-596` 注释）。
5. **`exec` 超时直接 SIGKILL**：上游 SIGTERM + 5s 后 SIGKILL 升级
   （exec.ts:50-58）；pir 超时直接按 pid SIGKILL
   （`crates/pir/src/core/extension_actions.rs:369-372` 注释登记；bash 工具的
   libc kill 助手为先例）。超时进程无缓冲冲刷机会，输出可能少于上游。
6. **（行为级，ADR-0007）非 RPC 模式 `ctx.command.*` 未绑定**：上游
   `bindCommandContext` 全模式接真实现（agent-session.ts:2309）；pir 仅 RPC 模式
   绑定（`crates/pir/src/modes/rpc.rs:707`），interactive / print 走未绑定默认值
   （`{cancelled: false}` / no-op，对齐上游 runner.ts:421-427 默认）。真实现
   `RuntimeCommandActions` 已就位（`core/extension_context.rs`），interactive
   绑定留后续版本。

## 影响面

扩展 API：第 1/3/4/5 条为同能力不同机制或边界约束，对外语义有定义良好的
对应物；第 2/6 条为 v0.1 功能缺口（见 ADR-0007）。协议 / session 格式 /
TUI 行为 / CLI 行为均不变。

## 处置

- **回写位置**：`02-design.md` §7.2 落地注记；`docs/extension-abi.md` §3
  method 表（既有注记）；本表 D-048 行；T15 任务文件偏离记录表
- **回写日期**：2026-08-08
- **ADR**：ADR-0007（第 2、6 条）；余不需要
