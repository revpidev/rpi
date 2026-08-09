# ADR-0004：平台原生助手不移植——macOS 原生修饰键与 Windows VT input 缺口

- **状态**：已采纳
- **日期**：2026-08-05
- **关联**：[ADR-0001](./0001-extension-and-config-dir.md)、[ADR-0002](./0002-baseline-decisions.md)、[`01-requirements.md`](../01-requirements.md) §8.6、[`02-design.md`](../02-design.md) §5、T11 偏离 D-016

## 背景

T11 移植 pi-tui 时发现上游两处依赖 **Node 原生 addon / 平台原生绑定** 的能力：

1. **macOS 原生修饰键检测**（`packages/tui/src/native-modifiers.ts`）：上游通过原生
   helper 查询 macOS 修饰键物理按下状态，用于 Apple Terminal 的 Shift+Enter 归一化
   （`\r` 在按住 Shift 时重写为 `\x1b[13;2u`，terminal.ts 调用点）。
2. **Windows VT input**（`packages/tui/src/terminal.ts` 加载 `win32-console-mode.node`）：
   设置 `ENABLE_VIRTUAL_TERMINAL_INPUT`，使 Windows 上 Shift+Tab 等组合以转义序列到达。

rpi 的技术边界（编码规范 §8.1）只允许 crossterm 做终端 I/O，不引入 Node 原生绑定体系；
为这两个平台特性各写一个 Rust 原生助手（macOS CGEvent / Windows console mode）超出
v0.1 范围。

## 决策

v0.1 **不移植**这两个原生助手，接受以下功能缺口：

- `is_native_modifier_pressed` 恒返回 `false`（等价于上游 addon 缺失时的已定义回退行为）。
  后果：Apple Terminal 的 Shift+Enter 归一化在 macOS 上**不生效**（输入保持 `\r`）。
- Windows 不设置 `ENABLE_VIRTUAL_TERMINAL_INPUT`（crossterm raw mode 不含此标志）。
  后果：Windows 上 Shift+Tab 仍以 `\t` 到达，而非 `\x1b[Z` / CSI-u 序列。

理由：

- 两处缺口仅在对应平台（macOS / Windows）可观察；Linux 对拍环境行为与上游**完全一致**
  （上游在 addon 缺失/非目标平台时走的正是同一条回退路径）。
- 上游原生 helper 本身是可选加载（加载失败即回退），本决策相当于恒定处于回退分支，
  不产生新的行为分叉面。
- 调用点逻辑（`normalize_apple_terminal_input`、平台/架构门控）已逐行保留，后续若补
  原生助手（macOS `core-graphics` / Windows console API）可直接接线，无需改解析层。

## 后果

- `01-requirements.md` §8.6 对应条目标注本缺口；T11 偏离 D-016 登记两条行为级差异并
  引用本 ADR。
- 平台支持矩阵文档化时（v0.1 发布说明）需注明：macOS Apple Terminal Shift+Enter、
  Windows Shift+Tab 为已知差异。
- 若未来补全原生助手，须补对应平台的行为对拍测试并关闭 D-016 对应条目。
