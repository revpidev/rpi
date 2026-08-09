# T31：全屏渲染器

- **状态**：未开始
- **里程碑**：M5
- **依赖**：T30
- **上游对照**：`packages/tui/src/tui-alt-screen.ts`（1047 行）、`components/alt-screen-flash.ts`；commit 链 `c13ffe187`（初版）、`17090d4b9`（flash）、`3c717842e`（导航）、`a3e93ec85`（半页）、`58fc0431a`+`171c6b520`（多击选择）、`c96bfaccd`（win32 右键粘贴）、`ebf33c0c2`（flash Copied）、`3c5a1b239`（滚轮步长 1）、`fc3554e16`（button-motion 1002）、`696a828a4`（焦点消费）、`af187eee4`/`73414d08b`/`a8ee03b81`/`4c01c7093`/`2c233a5c0`（Kitty/iTerm2）；测试：`test/tui-alt-screen.test.ts`（1067 行，30+ 场景）
- **需求章节**：v0.11 需求 R5.2.1、R5.2.3–R5.2.6；设计 §4.3
- **预估**：0.8 人月（全版本单任务最大）

---

## 目标

实现 `TuiAltScreen` 全屏渲染器：终端控制、应用自有滚动、鼠标交互、选择与剪贴板、
Kitty 图片管理、退出重打。验收蓝本为上游 1067 行 30+ 场景测试。

## 范围

### In

- **终端控制**：`\x1b[?1049h/l` 进出、禁用/恢复自动换行（`\x1b[?7l/h`）、鼠标序列启用/禁用（1000/1002/1003/1004/1006；tmux/Zellij/Screen 检测用 button-motion 1002）、focus in/out（`\x1b[I/O`）消费、同步输出包裹、失焦取消活动选择
- **滚动**：follow 输出、手动滚动位置保持、滚轮步长默认 1 行（`wheel_scroll_lines` 可配）、链式 overscroll（接 T30 ScrollView）、半页滚动 action
- **导航**：PageUp/Down（4 行重叠）、Home/End 文档首尾、OSC 133 语义 prompt 跳转（`ctrl+shift+up/down`，扫描 `\x1b]133;A`）
- **选择**：字符/词/行粒度；双击选词、三击选行（`DOUBLE_CLICK_INTERVAL_MS=500`）；按粒度拖动扩展；grapheme 边界吸附；边缘自动滚动（50ms interval）；OSC 8 URL 点击激活
- **剪贴板**：OSC 52 `\x1b]52;c;base64\x07` 复制 + flash 确认（"Copied!"）；win32 右键粘贴（bracketed paste 注入聚焦组件）
- **flash 通知**：右下角堆叠、超时逐个消失
- **Kitty 图片**：全局元数据注册表（LRU 1000 条）、placement-only 重发（`getKittyImagePlacement`）、像素级裁剪（`cropKittyImageLine`，`y=/h=/r=`）、离屏缓存（16 张/32MB 传输/64MB 解码上限）+ 淘汰发 `deleteKittyImage`、跳过裁剪行扫描；iTerm2 payload 补 `size=` 与清屏处理
- **逐行差分渲染**（`\x1b[{row};1H\x1b[2K`）+ 整行 box 引用优化（与 T28 性能项协同）
- **退出语义**：文档逐行 `\r\x1b[2K` 重打主屏（剥 OSC 133 前缀、恢复自动换行），或 `preserve_screen` 直接退出
- keybindings：`tui.altScreen.*` 8 个 action 注册与全屏遮蔽（裸 pageUp/pageDown/home/end 不到达编辑器）

### Out

- CLI `/settings` 热切换与设置项接线（T32）
- Mermaid/LaTeX 在全屏的专项调优（T29 交付的组件经布局引擎自然渲染）

## 开发要点

- 以 `tui-alt-screen.test.ts` 30+ 场景为移植主干：先 VT 测试架（pir-test-support 的 VirtualTerminal/RecordingTerminal 等价物扩充鼠标/OSC 序列注入），再逐场景实现
- Kitty 缓存/裁剪/回收是独立子模块（`kitty_registry`），单元测试先行
- 鼠标解析状态机（SGR/X10/残片消费）单独黄金化
- 本任务期间 main-screen 行为不得变化（T28 基线保持绿）

## 进度跟踪

- [ ] 设计细化
- [ ] 实现
- [ ] 自测
- [ ] 门禁验收
- [ ] 文档回写

## 自测清单

- [ ] `tui-alt-screen.test.ts` 30+ 场景意图全移植通过（滚动/wheel 路由/滚动条拖拽/多击选择/OSC 133 跳转/Kitty 缓存裁剪回收/iTerm2 清理/URL 点击/flash 堆叠/退出重打）
- [ ] 多路复用器检测（tmux/Zellij/Screen）→ button-motion 1002
- [ ] 键位遮蔽：全屏下裸导航键由视口消费；编辑器别名键（ctrl+home 等）仍可达
- [ ] 退出重打主屏内容 = 全屏最终文档（OSC 133 前缀剥离断言）
- [ ] main-screen 基线全绿（无副作用回归）

## 门禁验收

通用门禁 G1–G7 全过（G3 强制：VT 帧级对拍）。

任务特有标准：

- [ ] 需求 R5.2 五条逐条核对表
- [ ] 30+ 场景移植清单（上游场景 → pir 测试锚点）

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| （待登记） | | |

## 验收记录

（按 gates §3 模板填写）
