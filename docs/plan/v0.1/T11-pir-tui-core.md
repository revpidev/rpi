# T11：pir-tui 核心引擎

- **状态**：未开始
- **里程碑**：M5
- **依赖**：T01
- **上游对照**：`packages/tui/src/{tui,terminal,keys,stdin-buffer,keybindings}.ts`、`src/components/{text,container,spacer,truncated-text,box,loader,cancellable-loader}.ts`、`docs/tui.md`、`docs/terminal-setup.md`、`docs/tmux.md`（后两份为逐条对拍级基准）
- **需求章节**：§8.6（引擎部分）
- **预估**：3–4 人月（M5 共 8–11，与 T12 合计）

---

## 目标

移植 pi-tui 的渲染与输入引擎：ANSI 行列表 + 全量/差分渲染 + CSI 2026 + Kitty/legacy
键位解析，为 Interactive 模式提供与 Pi 行为一致的底座。

## 范围

### In

- crossterm 后端：raw mode、读写、尺寸（**不引入 ratatui**，设计文档 §5.1）
- `Component`（`render(width) -> Vec<String>` 行宽硬约束、`invalidate()`）/ `Focusable`（`handle_input`、`focused`、`wants_key_release`）trait；`Tui` 容器（children / overlays / focus / previous_lines / viewport）
- 渲染管线（编码规范 §8.3，步骤不得重排）：
  1. CSI 2026 包裹（`?2026h`/`?2026l`）
  2. 首次全量（不清屏）/ 全量清屏（`\x1b[2J\x1b[H\x1b[3J`）/ 行差分（**append 快路径、纯删除快路径、无变化只移硬件光标**）
  3. **全量回退条件全集**：宽度变化、高度变化（**Termux 例外**）、`clearOnShrink` 收缩、`first_changed < prev_viewport_top`、删除行数超终端高度、`request_render(force)`
  4. 16ms 节流
  5. 行尾 SGR + OSC 8 reset
  6. Kitty 图像行范围 expand + delete
- 调试通道：`PIR_DEBUG_REDRAW`（记录全量重绘原因）、`PIR_TUI_WRITE_LOG`
- 输入：`StdinBuffer`（CSI/OSC/DCS/APC/鼠标跨 chunk 重组 + bracketed paste 缓冲）→ 键位解析（**Kitty flags=7 含 key release/repeat** + legacy 全表；ctrl+symbol 与 ASCII 重叠处理）→ 全局 listener → focused 组件；**DA 探测无 Kitty 应答立即回退 modifyOtherKeys**（无超时等待）；退出前 `drain_input()` 防序列泄漏
- `KeybindingsManager`：读 JSON 映射到 editor/action 枚举，token 名与 Pi 一致（含旧键名迁移表，T09 提供数据）；**禁止硬编码键位**（例外：shift+ctrl+d = /debug）
- Overlay 栈：`composite_overlays` 合成后差分；9 种 anchor + offset/百分比/min/max/margin/`visible()`；`OverlayHandle`（focus/unfocus/setHidden/hide）；focus 恢复状态机
- IME：`CURSOR_MARKER` 零宽 APC 序列定位硬件光标（默认隐藏；`showHardwareCursor`/`PIR_HARDWARE_CURSOR=1`）；容器传播 focused
- 基础组件：`Text` / `Container` / `Spacer` / `TruncatedText` / `Box` / `Loader` / `CancellableLoader`
- 宽度工具：grapheme 宽度（`unicode-width` + ANSI 感知包装）
- 终端状态恢复：进入保存、退出/panic/信号恢复（panic hook 先恢复终端再输出，编码规范 §8.5）
- 终端特例处理框架（按上游逻辑移植）：Windows Terminal（Ctrl+Backspace 启发、VT input）、tmux（modifyOtherKeys 兼容、OSC 8 探测）、Apple Terminal（Shift+Enter 归一化、原生修饰键检测）、Termux（高度变化不全量重绘）、Ghostty（`shift+enter=\n`）、WezTerm（kitty_keyboard Escape 特例）
- 终端自省（tui.ts:686,1689-1716、terminal.ts:11-13,511-520）：OSC 11 背景色查询（`\x1b]11;?\x07`）、CSI ?996n 配色模式查询（`\x1b[?996n`）、CSI 16t 像元查询（`\x1b[16t`）、OSC 9;4 任务栏进度上报（`\x1b]9;4;3\x07` indeterminate / `\x1b]9;4;0;\x07` clear，indeterminate 期间 1s keepalive，`TERMINAL_PROGRESS_KEEPALIVE_MS=1000`；受 `terminal.showTerminalProgress` 设置门控，主题检测链见 T09）

### Out

- 业务组件（Editor / SelectList / Markdown / SettingsList / Image / Input / Autocomplete，T12）
- Interactive 模式绑定（T12）

## 开发要点

- `VirtualTerminal`（T02）驱动帧级测试：断言 ANSI 序列子集（去 CSI 2026 抖动）
- 渲染各回退分支逐一构造触发条件测试（宽度/高度/收缩/viewport/超删除/force）
- 终端恢复是硬性正确性要求：所有退出路径逐条核对（正常、abort、错误、panic）
- tmux / terminal-setup 的转义序列按文档字节级对拍

## 进度跟踪

- [ ] 设计细化
- [ ] 实现
- [ ] 自测
- [ ] 门禁验收
- [ ] 文档回写

## 自测清单

- [ ] VirtualTerminal 帧对比：首次全量 / 清屏全量 / 行差分（append/删除/无变化三快路径）/ 六种全量回退条件
- [ ] 16ms 节流行为测试
- [ ] 行尾 SGR + OSC 8 reset 断言
- [ ] 键位解析：Kitty flags=7 各修饰键组合 + release/repeat + legacy CSI 回退 + DA 探测回退
- [ ] 键位全部来自默认表/JSON 配置，无硬编码（grep 检查 + 测试覆盖；shift+ctrl+d 例外登记）
- [ ] panic hook：人为 panic 后终端状态恢复（VT 断言）
- [ ] 宽度工具：CJK / emoji / 组合字符 / ANSI 包裹文本宽度正确
- [ ] overlay 合成与 focus 恢复状态机；CURSOR_MARKER 定位
- [ ] 终端自省：OSC 11 / CSI ?996n / CSI 16t 查询与响应解析（VT 模拟应答）；OSC 9;4 进度序列 + 1s keepalive 启停

## 门禁验收

通用门禁 G1–G7 全过。

任务特有标准：

- [ ] 渲染管线各步有测试锚点且顺序锁定
- [ ] 组件渲染快照黄金文件（Text/Container/Spacer/TruncatedText/Box/Loader）建立
- [ ] `tmux.md` / `terminal-setup.md` 字节序列对拍映射表（G3）
- [ ] 真机 smoke：至少本机一种终端人工验证无闪烁、键位可用（记录终端与结果）

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| — | （暂无） | — |

## 验收记录

（待填写，模板见 `gates.md` §3）
