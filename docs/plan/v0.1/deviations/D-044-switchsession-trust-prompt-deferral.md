# D-044：交互模式 switchSession 异 cwd 信任提示降级（行为级）

- **状态**：已关闭（T15 W7，2026-08-08）
- **关联任务**：T14（W4）
- **级别**：行为级偏离（ADR-0006）
- **发现日期**：2026-08-07

## 原文档约定

- 上游基准：`packages/coding-agent/src/modes/interactive/interactive-mode.ts:4816/4830`
  @ 0.82.1（2efa728）——`switchSession` 接受 `projectTrustContextFactory`，resume 到
  不同 cwd 的会话时在 TUI 内弹信任选择器，选定当场生效并重载资源。
- 需求 §7.8：信任决策链与提示口径。

## 实际实现与偏离原因

pir 的 `switch_session` 无 `projectTrustContextFactory` 对应参数；T12 的
`showSelector` 框架为 fire-and-forget，缺「异步弹选择器并等待结果」的桥接。
当前行为：异 cwd resume 走 headless 信任链（ask→false），渲染既有 untrusted
warning；用户经 `/trust` 写盘 + 重启后生效。同 cwd 重建命中 `trust_by_cwd`
缓存，与上游一致。

降级方向是「更不信任」，无误判信任的安全风险；单点改造 T12 选择器框架超出
T14 范围，故立 ADR-0006 接受降级并安排在 T15 接线（`CreateRuntimeOptions.
project_trust_context` 口子已留）。

## 影响面

交互模式内 switch/resume 到不同 cwd 的会话：信任弹窗不出现，资源按未信任集
加载，需 `/trust` + 重启恢复完整资源。headless 各模式与启动路径不受影响。

## 处置

- **ADR**：[ADR-0006](../../adr/0006-switchsession-trust-prompt-deferral.md)
- **回写位置**：T14 任务文件偏离表、deviations 登记表
- **回写日期**：2026-08-07
- **关闭条件**：T15 异步选择器桥接就位后接线 switchSession 信任弹窗 + 补测试

## 关闭记录（T15 W7，2026-08-08）

关闭条件已满足，偏离消除（switchSession 信任弹窗恢复上游行为）：

- `ProjectTrustContext` 新增 `select_async` 槽位与
  `resolve_project_trusted_async`（决策链与同步版相同，尾部抽公共的
  `apply_trust_selection`）；`crates/pir/src/core/trust_manager.rs`。
- `AgentSessionRuntime::switch_session` 增加
  `project_trust_context_factory` 参数（`Option<Arc<dyn Fn(&Path) -> ProjectTrustContext>>`），
  仅异 cwd 时调用（`crates/pir/src/core/agent_session_runtime.rs`）。
- interactive `handle_resume_command` 构造桥接
  `InteractiveUiBridge.select` 的 factory（`commands_selectors.rs`），
  TUI 内弹信任选择器、选定当场生效。
- 证据测试：`crates/pir/tests/extension_host_w7_test.rs` 的
  `w7_switch_session_cross_cwd_uses_async_trust_selector` 等。
