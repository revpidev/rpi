# D-016：pir-tui 核心引擎移植 Rust 落地差异（native 绑定缺失两功能缺口 / 定时器显式 deadline / 所有权与重入 / 组件实现细节）

> 复制本模板为 `D-NNN-<short-slug>.md` 并填写。登记说明见 [README.md](./README.md)。

- **状态**：已关闭
- **关联任务**：T11
- **级别**：行为级（2 条功能缺口，须立 ADR）+ 实现细节（其余）
- **发现日期**：2026-08-05

## 原文档约定

- 文档与章节：`docs/01-requirements.md` §8.6（TUI 引擎：渲染管线、输入、终端特例、终端自省四件套）、`docs/02-design.md` §5（pir-tui 设计：Component/Focusable、crossterm 后端、不引入 ratatui）、§12（模块映射表）、`docs/plan/v0.1/T11-pir-tui-core.md`（设计细化）
- 原文约定：渲染管线各步顺序锁定 + 16ms 节流 + 全量回退条件全集；键位全部来自默认表/JSON 配置无硬编码（shift+ctrl+d = /debug 为登记例外）；终端恢复是硬性正确性要求；tmux.md / terminal-setup.md 转义序列字节级对拍；终端特例框架按上游移植——Windows Terminal（Ctrl+Backspace 启发、VT input）、Apple Terminal（Shift+Enter 归一化、原生修饰键检测）、Termux、Ghostty、WezTerm 各特例。

## ⚠ 功能缺口（行为级，须立 ADR）

以下两条在**目标平台上改变 TUI 键位行为契约**（README §1 行为级定义：影响 TUI 行为对拍），
不属于「实现细节」；二者都不是主动的设计选择，而是原生绑定缺失导致的平台能力缺口，
与上游**同环境行为一致**（上游在无该能力时同样降级），但在 macOS / Windows 上与上游
**存在**绑定时的行为不一致。处置：登记为有意差异待立 ADR（或将平台能力补全后关闭）。

### 缺口 1：macOS 原生修饰键检测缺失（Apple Terminal Shift+Enter 归一化在 macOS 失效）

- 上游 `packages/tui/src/native-modifiers.ts` 加载 Node 原生 addon
  （`native/darwin/prebuilds/darwin-<arch>/darwin-modifiers.node`）查询 macOS 修饰键状态；
  `packages/tui/src/terminal.ts` 的 Apple Terminal 特例依赖它：Terminal.app 对
  `Shift+Enter` 仍发 plain `\r` 时，若本地检测到 Shift 按下，把 `\r` 归一化为
  `\x1b[13;2u`（terminal-setup.md §Apple Terminal）。
- pir 无等价原生绑定，`native_modifiers.rs::is_native_modifier_pressed` **恒返回 false**
  （= 上游 addon 不可用时的行为）。`terminal.rs::forward_input_sequence` /
  `normalize_apple_terminal_input` 已按上游接线（含 `is_apple_terminal_session`
  TERM_PROGRAM 判定），但归一化分支在 macOS 上永不触发。
- 行为级理由：在 macOS + Terminal.app 上，Shift+Enter 键位行为与上游不一致
  （shift+enter 收不到，退化为 enter）；影响 TUI 键位行为对拍契约。
- 平台范围：macOS 本地；远程 SSH 上游本就失效（fallback 只同机生效），无差异。
- 实现位置：`native_modifiers.rs`（整文件，100 行）、`terminal.rs` :46-50、:577-589、:148-162。
- 测试：无（恒 false 路径无行为可测）。

### 缺口 2：Windows VT input 缺失（Windows 上 Shift+Tab 仍是 `\t`）

- 上游 `packages/tui/src/terminal.ts` 加载原生 helper（`win32-console-mode.node`，
  terminal.ts:338-366）设置 `ENABLE_VIRTUAL_TERMINAL_INPUT`，使 Shift+Tab 等
  修饰键以转义序列到达应用（terminal-setup.md §Windows Terminal 依赖此能力）。
- pir 无等价原生 helper，且 crossterm raw mode 不设置该 flag，
  `terminal.rs` :41-45 登记：Windows 上 Shift+Tab 仍为 plain `\t`（与 Tab 不可区分）。
- 行为级理由：在 Windows 上，`shift+tab` 键位（如 `tui.select` 反向移动/补全反向循环）
  无法触发，与上游不一致；影响 TUI 键位行为对拍契约。
- 平台范围：仅 Windows（本仓库 Linux 对拍环境不可观察）。
- 实现位置：`terminal.rs` :41-45（注释登记，无实现）。
- 测试：无。

## 实际实现与偏离原因（实现细节）

以下各项不影响行为契约（模块内部结构 / 私有 API / 实现机制），按文件分组：

### terminal.rs / stdin_buffer.rs（定时器与 I/O 机制）

1. **定时器 → 显式 deadline**：上游 150ms `keyboardProtocolBufferFlushTimer`、
   1s `progressInterval`（OSC 9;4 keepalive）、16ms 渲染节流与 introspection
   查询超时全部改为显式 deadline（`next_flush_deadline` / `tick` / `pump` 由 TUI
   事件循环驱动，crossterm poll 带超时）；`process()` 驱动的输入到达重排 deadline，
   到期只触发一次——语义与上游一致（`stdin_buffer.rs` :11-19、`terminal.rs` :9-17）。
2. **输入路由**：上游 `process.stdin.on("data")` → 独立 stdin reader 线程
   （阻塞 `read()` + 增量 UTF-8 解码，镜像 Node `setEncoding("utf8")`），经
   `std::sync::mpsc` 转发；`pump()` 同步 `recv_timeout`（不用 tokio mpsc，
   因其 `blocking_recv` 在 async 上下文 panic 且无超时变体）；`drain_input()` 与
   SIGWINCH 转发用 tokio（`terminal.rs` :18-28）。
3. **Resize 差异**：unix 下 SIGWINCH 由 tokio task 转发（无 tokio runtime 时
   不投递 resize 事件）；上游 Windows libuv `resize` 与自 SIGWINCH 维度刷新
   （terminal.ts:152-156）未移植——crossterm `terminal::size()` 每次 ioctl
   查询，维度不会过期（`terminal.rs` :29-34）。
4. **stop() 差异**：上游 `process.stdin.pause()`（terminal.ts:446）无 Rust 对应，
   reader 线程保持阻塞在 `read()` 直到下一字节到达后 channel send 失败退出
   （`terminal.rs` :35-40）；`drain_input` 消费并丢弃排队事件（上游 side
   listener 只打时间戳，两者都不处理被吞字节，`terminal.rs` :51-54）。
5. **`drainInput` 返回 boxed `Future`** 保持 `Terminal` trait object-safe
   （`terminal.rs` :55-56）。
6. **Kitty flags 解析饱和 `u32::MAX`**（上游 JS double 不会溢出；唯一观察属性
   `flags !== 0` 保留，`terminal.rs` :63-64）。
7. **StdinBuffer EventEmitter → 返回值**：`data`/`paste` 事件改为
   `process()` 返回的 `StdinBufferEvent` 值；Buffer 输入路径（单字节 >127 →
   ESC + (byte-128)，否则 UTF-8 lossy）暴露为 `process_bytes()`（`stdin_buffer.rs` :11-22）。
8. **Kitty 重复码点丢弃按 BMP 语义**：单多字节字符按一个码点处理（上游 UTF-16
   码元语义对全部 BMP 输入一致）；Rust 字符串无法持有孤立代理，astral 字符整体
   丢弃（上游比较孤立代理半值时永不可能相等，`stdin_buffer.rs` :23-27）。

### keys.rs（解析实现机制）

9. **`_lastEventType` 省略**（上游 write-only，`isKeyRelease`/`isKeyRepeat` 用
   子串检查）；`ParsedKittySequence` 保留 `shifted_key`/`event_type` 镜像结构
   （`#[allow(dead_code)]`，keys.rs :4-8）。
10. **`Key` 单元结构体 + 关联常量/函数**（`Key.super(...)` → `Key::super_key(...)`，
    关键字避让）；`KeyId` 为 `&str`（TS template-literal 类型运行时擦除，keys.rs :9-12）。
11. **`\d` 正则组 → 手工字节游标解析 `i32`**；数值字段溢出 `i32` 解析为不匹配，
    与上游观察一致（终端不会发出此类序列，keys.rs :19-22）。
12. **Kitty 协议状态 `AtomicBool`**；`isWindowsTerminalSession` 读同名环境变量
    （`WT_SESSION`/`SSH_CONNECTION`/`SSH_CLIENT`/`SSH_TTY`，keys.rs :16-18）。

### tui.rs（所有权 / 重入 / 生命周期）

13. **组件身份**：`SharedComponent = Arc<Mutex<Box<dyn Component>>>`，身份比较
    `Arc::ptr_eq`；overlay 栈条目携带唯一 `id`（替代上游条目对象身份
    `restoreState.overlay === entry`）；`containsComponent` 走默认扩展方法
    `shared_children`（替代 `instanceof`，tui.rs :26-34）。
14. **重入处理**：`Tui` 为可克隆句柄包 `Arc<Mutex<TuiInner>>`；持锁期间（组件
    `handle_input`/`render` 内或他线程）的变更请求入队、当前 dispatch/render 完成后
    统一 drain，保持上游可观察顺序；只读查询锁争用返回默认值（tui.rs :35-42）。
15. **定时器显式化**（同上 1）：`request_render` 只记录意图，`tick`/`pump` 驱动；
    多次 `request_render(true)` 合并为一次渲染（上游每个 nextTick 回调跑一次
    `doRender`，tui.rs :43-49）。
16. **输入投递**：`Terminal::start` 回调推入 inbox，`tick` 中按上游 `handleInput`
    流程 drain——同线程同序，但投递发生在 `Terminal::pump` 之外，TUI 锁不会被
    terminal 等待事件持有（tui.rs :50-54）。
17. **`handle_input` 默认 trait 方法**：上游 `focusedComponent?.handleInput`
    存在性检查恒通过——无输入处理组件仍收到 no-op 调用 + 尾部 requestRender
    （tui.rs :55-58）。
18. **停止的渲染定时器修复**：上游在节流渲染 pending 时 `stop()` 泄漏
    `renderRequested === true`，吞掉重启后的渲染；此处 pending deadline 跨
    `stop()` 存活、`start()` 后首个 `tick` 触发（tui.rs :59-62）。
19. **`SizeValue` 枚举**；上游非法百分比字符串回退（`"abc%"` → anchor center）
    应用于负/NaN 百分比（tui.rs :71-72）。
20. **listener 句柄**：`add_input_listener` / `on_terminal_color_scheme_change`
    返回数值 id + `remove_*`（替代 unsubscribe 闭包，tui.rs :73-74）。
21. **查询返回 `oneshot::Receiver`**（替代 Promise；超时由 `tui::tick` 触发，
    tui.rs :75-77）。
22. **宽度溢出路径**：写 crash log → stop TUI → panic（上游 throw uncaught Error）；
    redraw/crash log 写失败忽略（tui.rs :78-81）。
23. **类型布局差异**（行为不变）：`Component`/`Focusable` trait 化（上游
    结构性 `"focused" in component` 检查 → `as_focusable`）；Loader 持
    `RenderHandle` 而非 `&TUI`；`Tui` 自己持有 children 并镜像 Container API
    （tui.rs :13-24）。

### keybindings.rs / native_modifiers.rs / terminal_image.rs / utils.rs / components/*.rs

24. **`Keybinding` 枚举镜像 keyof 联合**：上游 `Keybindings` 接口值全为 `true`
    字面量、无运行时形状；id 为 `tui.*` token 字节一致（keybindings.rs :4-9）。
25. **配置保序**：`KeybindingsConfig`/`KeybindingDefinitions` 用插入序容器，
    JSON 对象键序 round-trip；TS `undefined` 无 JSON 表示（缺键等价）；显式空
    数组解绑（keybindings.rs :10-14）。
26. **单例可替换槽 `RwLock<Option<&'static RwLock>>`**：`set_keybindings`
    每次安装新实例、末装生效（与上游无条件赋值一致，keybindings.ts:235-237）；
    被替换的旧实例按安装泄漏（与上游丢弃旧引用由 GC 回收同构，每次泄漏一个小
    分配，安装次数受启动/会话切换约束，keybindings.rs :15-22）。
27. **JSON null 键值**：反序列化为空键列表（上游存 null、匹配时 TypeError
    崩溃，keybindings.rs :22-24）；`get_definition` 返回 `Option`
    （上游返回类型 unsound，keybindings.rs :25-28）。
28. **`ModifierKey` 枚举** + `name()` 返回上游字节值（native_modifiers.rs :11-13）。
29. **`probeTmuxHyperlinks` 用 `try_wait` 轮询**：250ms 预算内 kill 卡死的 tmux，
    与上游 `execSync` timeout 语义一致（terminal_image.rs :7-10）；`allocateImageId`
    用 `RandomState`（OS 熵）+ 计数器替代 `Math.random()`（同范围分布，
    :11-13）；Base64 用严格引擎（Node 忽略非字母表字符，脏输入返回 None，
    :14-17）；`encodeITerm2` 的 `number|string` 参数建模为 `Option<String>`
    （:18-19）。
30. **Unicode 基元差异**：上游正则跑在 ICU 76（Node 24）上，pir 用运行时探测
    + Unicode 16.0 官方文件核验生成的静态表（utils.rs :7-19）；`Intl.Segmenter`
    → `unicode-segmentation` 1.13.x（UAX #29，全语料对 ICU 76 核验，:20-25）；
    ANSI strip 按字符而非 UTF-16 码元（等价，:26-29）；宽度缓存
    `Mutex<HashMap>` + 插入序队列（淘汰策略不可观察，:30-32）；
    `extract_segments` 每次新建 `AnsiCodeTracker`（语义等价，:33-35）；
    `is_whitespace_char` 精确实现 ECMA-262 `\s`（含 U+FEFF、排除 U+0085，
    :36-37）。
31. **Text/Box 渲染缓存 `RefCell`**（`render(&self)` 需要内部可变性；Send 非
    Sync，匹配单线程渲染循环，text.rs :4-6、box.rs :4-6）；颜色回调
    `Box<dyn Fn(&str) -> String + Send + Sync>`（text.rs :7-8）；Box 组件
    `StdBox` 别名避让 std 遮蔽（box.rs :9-11）。
32. **Loader 定时器线程化**：`setInterval` → 专用线程 + stop channel
    `recv_timeout`，`stop()`/`Drop` join（无后台泄漏，编码规范 §6.4；上游显式
    stop 前 interval 一直运行，loader.rs :7-11）；帧状态 `Arc<AtomicUsize>`，
    render 时现算文本（渲染字节相同，:12-15）；构造器持 `RenderHandle` 而非
    整个 TUI（:4-6）。
33. **CancellableLoader AbortSignal**：DOM `AbortController`/`AbortSignal` →
    Rust `Arc` + atomic flag + 监听列表（abort 幂等、监听同步按注册序触发，
    cancellable_loader.rs :5-9）；Escape 取消经 `Keybinding::SelectCancel` 走
    KeybindingsManager，无硬编码键位（:10-12）。

## 环境变量 `PIR_` 前缀改名（不算偏离，说明依据）

pir-tui 全部环境变量按 ADR-0001 改名（tui.rs :63-70、terminal.rs :57-62）：

| 上游 | pir | 依据 |
|------|-----|------|
| `PI_HARDWARE_CURSOR` | `PIR_HARDWARE_CURSOR` | ADR-0001 §2（环境变量前缀 `PIR_*`） |
| `PI_CLEAR_ON_SHRINK` | `PIR_CLEAR_ON_SHRINK` | 同上 |
| `PI_DEBUG_REDRAW` | `PIR_DEBUG_REDRAW` | 同上 |
| `PI_TUI_DEBUG` | `PIR_TUI_DEBUG` | 同上 |
| `PI_CODING_AGENT_DIR` | `PIR_CODING_AGENT_DIR` | 同上 |
| `PI_TUI_WRITE_LOG` | `PIR_TUI_WRITE_LOG` | 同上（terminal.rs :96，与 `pir::core::environment` 镜像，pir-tui 不依赖 pir crate） |

同族路径/文件改名（同为 ADR-0001）：默认日志目录 `~/.pir/agent`（上游 `~/.pi/agent`）、
`pir-debug.log`/`pir-crash.log`（上游 `pi-debug.log`/`pi-crash.log`）、`PIR_TUI_DEBUG`
dump 目录 `/tmp/pir-tui`（上游 `/tmp/tui`）。terminal.rs 写日志时间戳目录名用 UTC
（无本地时区 crate 依赖，terminal.rs :57-59）。

## 影响面

TUI 行为 / 无（纯内部）双类标注：

- **行为级（TUI 行为）**：缺口 1 与缺口 2 —— 仅在 macOS（Terminal.app Shift+Enter
  归一化）与 Windows（Shift+Tab）目标平台上与上游不一致；Linux 对拍环境不可观察，
  不影响现有 VirtualTerminal 帧对拍与 441 项测试。
- **无（纯内部）**：其余全部条目 —— 定时器/IO 机制、所有权与重入、类型布局、
  组件实现细节均为模块内部结构或私有 API 变化，可观察行为（渲染字节、事件序、
  输入语义）以测试锚定与上游一致（见 `T11-tmux-terminal-setup-mapping.md` 对拍表）。

## 处置

- **回写位置**：`docs/01-requirements.md` §8.6（终端特例小节「native 绑定缺失」注记）、
  `docs/02-design.md` §5（§5.2 核心抽象落地：默认方法 / SharedComponent / 重入队列 /
  定时器显式化；§5.3 调试环境变量；§5.4 `PIR_` 前缀与显式 deadline；§5.5 组件落地标注；
  §5.6 终端状态恢复）、§12（映射表 tui.rs / terminal.rs / stdin_buffer.rs / keys.rs /
  keybindings.rs / native_modifiers.rs / terminal_colors.rs / terminal_image.rs /
  fuzzy.rs / utils.rs / recovery.rs / components 逐文件行）、`docs/coding-standards.md`
  §8.2（trait 草图同步）、`docs/plan/v0.1/T11-pir-tui-core.md`（偏离记录表 + 自测清单）
- **回写日期**：2026-08-05
- **ADR**：行为级两条 → [ADR-0004](./adr/0004-platform-native-helper-gaps.md)（已采纳）；
  实现细节「不需要」
