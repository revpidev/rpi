# ADR-0006：交互模式 switchSession 异 cwd 信任提示降级为 headless 判定

- **状态**：已采纳
- **日期**：2026-08-07
- **关联**：[`01-requirements.md`](../01-requirements.md) §7.8、T14 偏离 D-043（遗留）、D-044

## 背景

上游交互模式内 `switchSession` 接受 `projectTrustContextFactory`
（`packages/coding-agent/src/modes/interactive/interactive-mode.ts:4816/4830` @ 0.82.1）：
resume 到**不同 cwd** 的会话时在 TUI 内弹信任选择器，选定后立即生效并重载资源。

T14（W4）接线信任决策链时发现：pir 的 `switch_session` 无此参数，T12 的
`showSelector` 框架为 fire-and-forget，缺少「异步弹选择器并等待结果」的桥接变体。
在 T14 内为该单点改造 T12 选择器框架超出本任务范围。

## 决策

v0.1 接受以下降级行为（D-044 按行为级偏离登记）：

- 交互模式内 switch/resume 到不同 cwd 时，信任判定走 headless 链（已存条目/默认值之外
  ask→false），随后渲染既有的 untrusted warning（提示 `/trust` 并重启生效）。
- 同 cwd 的会话重建命中 `trust_by_cwd` 缓存，行为与上游一致（不受影响）。
- `/trust` 本身只写不重载（上游同），故降级路径的最终可用性与上游的差距仅为：
  上游可当场弹窗当场生效，pir 需一次 `/trust` + 重启。

理由：

- 有定义良好的回退路径（warning + `/trust`），不产生误判为信任的安全风险
  （降级方向是「更不信任」）。
- 接线口子已留：`CreateRuntimeOptions.project_trust_context` 字段就位（W4），
  T15 扩展宿主落地时需异步事件桥接（`emit_project_trust` 已是 async trait 方法），
  届时一并为 switchSession 路径接上 TUI 选择器。

## 后果

- T15 任务验收时须复核本 ADR：扩展宿主的异步选择器桥接就位后，为 switchSession
  异 cwd 路径接上信任弹窗并补测试，关闭 D-044。
- v0.1 发布说明需注明该已知差异。
