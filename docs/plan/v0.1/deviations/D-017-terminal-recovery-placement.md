# D-017：终端状态恢复落位 rpi-tui `recovery.rs`（上游在 coding-agent interactive-mode 层）

- **状态**：已关闭
- **关联任务**：T11
- **级别**：实现细节偏离
- **发现日期**：2026-08-05

## 原文档约定

- 文档与章节：`docs/02-design.md` §5（rpi-tui 设计）、`docs/coding-standards.md` §8.5
  （终端状态恢复：进入 TUI 时保存终端状态，退出 / panic / 收到信号时**必须恢复**；
  安装 panic hook 先恢复终端再走默认 panic 输出）
- 原文约定：终端恢复是硬性正确性要求；语义来自上游
  `packages/coding-agent/src/modes/interactive/interactive-mode.ts` 的 `uncaughtCrash`
  / `registerSignalHandlers`（Node 进程级回调，interactive-mode.ts:3613-3683）。

## 实际实现与偏离原因

终端恢复语义落位 `crates/rpi-tui/src/recovery.rs`（`install_panic_hook` /
`restore_terminal` / `spawn_signal_restore`），而非上游所在的 coding-agent
interactive-mode 层：

1. **放置差异**：上游把信号/异常恢复接线在 coding-agent 层，因为 Node 的
   signal/exception handler 是注册在事件循环旁的进程级回调；rpi 将恢复归 rpi-tui——
   编码规范 §8.5 把终端恢复指派给 TUI 层，且 Rust panic hook 是进程级状态，
   必须捕获 live `Tui` 句柄。graceful-shutdown 编排（扩展清理、`drainInput`、
   session 关闭事件）仍留 interactive mode（T12），与上游拆分一致。
2. **panic 后不退出进程**：上游 `uncaughtCrash` 为 `process.exit(1)`；Rust panic
   hook 恢复终端后继续 unwind（main 退出码 101）。exit 也会杀掉无关工作线程上的
   panic，Rust 语义刻意不这样做。
3. **信号恢复 exit 0**：`spawn_signal_restore`（SIGTERM/SIGHUP）恢复终端后
   `process.exit(0)`，对齐 interactive-mode `shutdown({fromSignal:true})`
   （interactive-mode.ts:3646-3663）。
4. **Rust 新增回退**：panic 线程持 `Tui` 锁时（渲染中途 panic），`restore_terminal`
   回退固定恢复字节序列（`\x1b[?2031l\x1b[?2004l\x1b[<u\x1b[>4;0m\x1b[?25h` +
   raw mode 复位）直写 stdout，避免死锁；上游单线程事件循环不存在此情形。

## 影响面

无（纯内部）。恢复的语义与上游一致：先恢复终端再走错误输出 / 退出；差异仅在接线
层与进程退出路径（101 vs 1），不改变 TUI 行为契约；§8.5 的退出路径核对（正常退出、
abort、错误、panic）已由 T11 自测覆盖。

## 处置

- **回写位置**：`docs/02-design.md` §5.6（终端状态恢复小节）、§12（映射表
  interactive-mode.ts → recovery.rs 行）
- **回写日期**：2026-08-05
- **ADR**：不需要
