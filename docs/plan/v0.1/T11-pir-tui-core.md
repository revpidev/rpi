# T11：rpi-tui 核心引擎

- **状态**：已完成
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
- 调试通道：`RPI_DEBUG_REDRAW`（记录全量重绘原因）、`RPI_TUI_WRITE_LOG`
- 输入：`StdinBuffer`（CSI/OSC/DCS/APC/鼠标跨 chunk 重组 + bracketed paste 缓冲）→ 键位解析（**Kitty flags=7 含 key release/repeat** + legacy 全表；ctrl+symbol 与 ASCII 重叠处理）→ 全局 listener → focused 组件；**DA 探测无 Kitty 应答立即回退 modifyOtherKeys**（无超时等待）；退出前 `drain_input()` 防序列泄漏
- `KeybindingsManager`：读 JSON 映射到 editor/action 枚举，token 名与 Pi 一致（含旧键名迁移表，T09 提供数据）；**禁止硬编码键位**（例外：shift+ctrl+d = /debug）
- Overlay 栈：`composite_overlays` 合成后差分；9 种 anchor + offset/百分比/min/max/margin/`visible()`；`OverlayHandle`（focus/unfocus/setHidden/hide）；focus 恢复状态机
- IME：`CURSOR_MARKER` 零宽 APC 序列定位硬件光标（默认隐藏；`showHardwareCursor`/`RPI_HARDWARE_CURSOR=1`）；容器传播 focused
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

- [x] 设计细化
- [x] 实现
- [x] 自测
- [x] 门禁验收
- [x] 文档回写

## 自测清单

- [x] VirtualTerminal 帧对比：首次全量 / 清屏全量 / 行差分（append/删除/无变化三快路径）/ 六种全量回退条件（`tui::tests`，tui-render.test.ts 24 例全移植；`extraLines > height` 分支与「删除行使 viewport 上移」互斥无法构造，上游亦无测试，属防御性代码，已逐行移植）
- [x] 16ms 节流行为测试（`tui::tests::render_requests_are_throttled_to_*`）
- [x] 行尾 SGR + OSC 8 reset 断言（`tui::tests` 引擎补充用例 + style-leak 回归）
- [x] 键位解析：Kitty flags=7 各修饰键组合 + release/repeat + legacy CSI 回退 + DA 探测回退（`keys::tests` 61 例 + `terminal::tests` 协商 harness 7 例）
- [x] 键位全部来自默认表/JSON 配置，无硬编码（grep 核对 4 处命中全部合规：shift+ctrl+d 为登记例外，余为协议级解析，见验收记录 G4）
- [x] panic hook：人为 panic 后终端状态恢复（VT 断言）
- [x] 宽度工具：CJK / emoji / 组合字符 / ANSI 包裹文本宽度正确（`utils::tests` 38 例 + 开发期 163 万行运行时对拍字节一致）
- [x] overlay 合成与 focus 恢复状态机；CURSOR_MARKER 定位（overlay 系列 70+ 例、`tui::tests::cursor_marker_positions_hardware_cursor_and_is_stripped`）
- [x] 终端自省：OSC 11 / CSI ?996n / CSI 16t 查询与响应解析（VT 模拟应答）；OSC 9;4 进度序列 + 1s keepalive 启停（`terminal::tests::set_progress_*`）

## 门禁验收

通用门禁 G1–G7 全过。

任务特有标准：

- [x] 渲染管线各步有测试锚点且顺序锁定（`TuiInner::do_render` 步骤注释锚定上游行号，tui-render.test.ts 24 例）
- [x] 组件渲染快照黄金文件（Text/Container/Spacer/TruncatedText/Box/Loader）建立
- [x] `tmux.md` / `terminal-setup.md` 字节序列对拍映射表（G3）
- [x] 真机 smoke：至少本机一种终端人工验证无闪烁、键位可用（记录终端与结果）（本轮为 pty smoke，见验收记录；人工交互验证待 T12 联机补）

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| D-016 | rpi-tui 核心引擎 Rust 落地差异（macOS 原生修饰键、Windows VT input 两功能缺口 + 定时器/所有权/组件实现细节；环境变量 `RPI_*` 改名依 ADR-0001 不算偏离） | 已回写 |
| D-017 | 终端恢复语义落位 rpi-tui（`recovery.rs`），上游在 coding-agent interactive-mode 层（Node 进程级回调，interactive-mode.ts:3613-3683）；panic hook 恢复后不退出进程（Rust 继续 unwind，main 退出码 101；上游 `uncaughtCrash` 为 `process.exit(1)`）；信号恢复退出码 0 对齐 interactive-mode `shutdown({fromSignal:true})` | 已回写 |

## 验收记录

- 验收日期：2026-08-05
- 验收人：kimi（主代理自证，按 gates.md §3 逐项；实现由子代理完成、主代理复核）
- G1 构建/静态检查：通过。`cargo build --workspace` ✓、`cargo clippy --workspace --all-targets -- -D warnings` ✓、`cargo fmt --all -- --check` ✓
- G2 测试：通过。`cargo test --workspace`：1809 passed / 0 failed / 0 ignored（其中 rpi-tui：lib 443 + snapshots 集成 11 + doc-test 1）。说明：本轮曾出现 1 次偶发失败 `rpi::cli::file_processor::tests::test_empty_file_is_skipped`（T10 既有代码，T11 未触碰 rpi crate）；该用例单跑通过、rpi lib 连跑 3 次通过、最终全量复跑通过，判定为与 T11 无关的并发 flake，留待后续观察。
- G3 对拍：通过。
  - `tmux.md` / `terminal-setup.md` 逐条对拍映射表：[`T11-tmux-terminal-setup-mapping.md`](./T11-tmux-terminal-setup-mapping.md)（31 条：有测试锚点 21、同一解析路径变体无直接断言 9、纯说明 1；字节序列原样 + 实现位置精确到文件/函数）
  - VirtualTerminal 帧级对拍：tui-render.test.ts 24 例、overlay 系列（options 24 / non-capturing 44 / short-content / shrink / cell-size-input / overlay-style-leak）、viewport-overwrite-repro、regression-overlay-cjk-boundary、tab-width TUI 集成例、OSC 11 查询 5 例全移植
  - 组件渲染快照黄金 11 例：`crates/rpi-tui/tests/snapshots.rs` + `tests/snapshots/*.snap`（更新机制 `RPI_UPDATE_SNAPSHOTS=1`）
  - 开发期补充验证：utils 与上游 Node 24 运行时对拍 1,637,661 行断言字节一致（两轮随机种子）；Unicode 生成表以官方 UCD/emoji 16.0 文件核验，生成器已入库 `scripts/gen-tui-unicode-data.py`
- G4 红线：通过。`external/pi` 无改动（HEAD `2efa728d2ee90ef597626e96b1e28ef2b279f07c`）；未引入 JS/TS 执行能力；未读写 `~/.pi`/`.pi`；未引入 SQLite；token 估算未触碰；非测试代码无 `unwrap()`/`expect()`（各模块交付时 grep 核验）；日志/错误无凭据；键位无硬编码（grep 命中 4 处全部合规：`tui.rs` shift+ctrl+d 为任务书登记例外、CancellableLoader 走 KeybindingsManager、余 2 处为协议级解析/重写）；范围排除项未引入
- G5 线格式：通过。T11 无新增 JSONL/RPC 线格式；keybindings JSON 配置的 serde 形状（插入序保持、空数组=解绑）有测试锚定（`keybindings::tests`）
- G6 文档同步：通过。全部移植文件带溯源注释（上游路径 + 2efa728 + Intentional differences）；回写 `02-design.md` §5.2/§5.3/§5.4/§5.5/§5.6（新增）/§12、`01-requirements.md` §8.6、`coding-standards.md` §8.2；新增 ADR-0004
- G7 偏离闭环：通过。D-016（33 项汇总，其中 macOS 原生修饰键、Windows VT input 两条功能缺口判行为级，已立 ADR-0004）、D-017（终端恢复落位 rpi-tui recovery.rs）均已登记 + 回写，门禁后转「已关闭」
- 任务特有标准：渲染管线测试锚点 ✓ / 快照黄金 ✓ / 映射表 ✓ / 真机 smoke —— 本环境无交互终端，以 `script(1)` pty smoke 代替：`examples/tui_smoke`（启动渲染 CSI 2026 + 差分动画正常；按 q / 超时 / SIGTERM 三条退出路径均 exit 0 且恢复序列完整）与 `examples/tui_panic_restore`（panic 后恢复序列先于 panic 消息写出，exit 101）。**人工交互验证（无闪烁、键位可用）待 T12 联机时补验**。
- 结论：通过（人工 smoke 一项按上注记挂起到 T12）

### 交付摘要

- 移植模块（镜像 `packages/tui/src/`）：`tui.rs`（TUI 核心：渲染管线六步、六种全量回退、16ms 节流、Kitty 图像行 expand/delete、overlay 栈 + focus 恢复状态机、CURSOR_MARKER 硬件光标、OSC 11/?996n/16t 自省）、`terminal.rs`（Terminal trait + ProcessTerminal：Kitty 协商 + DA 立即回退 modifyOtherKeys、drain_input、OSC 9;4 + keepalive、终端特例）、`stdin_buffer.rs`（跨 chunk 重组 + bracketed paste）、`keys.rs`（Kitty flags=7 + legacy 全表）、`keybindings.rs`（31 条定义 + 全局访问器）、`native_modifiers.rs`、`terminal_colors.rs`、`terminal_image.rs`（Kitty/iTerm2 编码 + 能力检测矩阵）、`utils.rs`（grapheme 宽度 + ANSI 包装）、`recovery.rs`（panic hook + 信号恢复）、`components/`（text/spacer/truncated_text/box/loader/cancellable_loader）
- 关键设计适配：组件存储 `SharedComponent = Arc<Mutex<Box<dyn Component>>>`（`Arc::ptr_eq` 同一性）；`Tui = Arc<Mutex<TuiInner>>` 重入 pending 队列；定时器显式 deadline 化（`next_flush_deadline`/`tick`/`pump`）；Loader 持 `RenderHandle` 替代 `ui: TUI`；环境变量 `RPI_*` 前缀（ADR-0001）
- 测试基线：rpi-tui lib 453（移植上游测试意图：keys 61、stdin_buffer 57、terminal 19、keybindings 17、tui 引擎 123、utils 38、组件 48、terminal_colors/image 等）+ snapshots 11 + doc-test 1
