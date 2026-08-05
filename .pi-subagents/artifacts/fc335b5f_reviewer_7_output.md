# Review: pir-tui components + recovery.rs（T11 移植审查）

范围：`crates/pir-tui/src/components/{text,spacer,truncated_text,box,loader,cancellable_loader}.rs`（约 1600 行）、`src/recovery.rs`、`examples/`、`tests/snapshots.rs`；对照上游 `external/pi/packages/tui/src/components/*.ts` 与 `interactive-mode.ts`（commit 2efa728d，`uncaughtCrash`/`registerSignalHandlers`）。

> 注：任务指定读取的 `plan.md` / `progress.md` 在仓库中不存在（已用 `find` 确认）。改为对照 `docs/plan/v0.1/T11-pir-tui-core.md` 与 `docs/plan/v0.1/deviations/D-016/D-017` 交叉核对，并直接对照上游 TS 源码。根目录 `examples/` 为空，示例实际位于 `crates/pir-tui/examples/`。

## 验证方式
- 逐行对照上游 TS：text.ts / spacer.ts / truncated-text.ts / box.ts / loader.ts / cancellable-loader.ts / utils.ts（truncateToWidth、finalizeTruncatedResult、applyBackgroundToLine）/ interactive-mode.ts（signal + uncaughtCrash 路径）。
- 核对 11 个 snapshot golden 字节与组件输出。
- 检查 Tui 锁语义：`request_render(false)` 只碰 `schedule` 锁（tui.rs:1043-1048、1277-1293），Loader 动画线程不接触 `inner` 锁。
- 运行测试：`cargo test -p pir-tui --lib`（443 通过）、`cargo test -p pir-tui --test snapshots`（11 通过）、`cargo build -p pir-tui --examples`（通过）。

## Review
- **Correct（有证据）**：
  - `text.rs` 与 text.ts 逐分支一致：空文本早返回 `[]`、tab→3 空格、`content_width = max(1, width-2*px)`、非 bg 路径按 `visible_width` 补空格、`result.is_empty()→[""]` 防御分支（上游同款死分支，text.ts:101 vs text.rs:135-137）、缓存键 `(text, width)` 与失效路径（set_text/set_custom_bg_fn/invalidate）均对齐。Loader 复用 `render_text` 按文本值做键，语义正确。
  - `box.rs` 与 box.ts 一致：先渲染 children 再查缓存（缓存只省 bg/填充）、`bg_sample = bg_fn("test")` 探测 bgFn 变化、`set_bg_fn` 不失效（上游同款）、`remove_child` 按指针身份对应上游 `indexOf` 引用相等。
  - `loader.rs`：`render()` = `[""] + Text.render(display_text)`，与上游 `["", ...super.render(width)]` 一致；verbatim/着色、`indicator = frame.is_empty() ? "" : frame+" "` 均同上游 loader.ts:84-87；`set_indicator` 的 verbatim/帧/间隔默认值语义与上游一致；`display_text()` 越界取 `""` 对应 `frames[i] ?? ""`。
  - **定时器线程生命周期经核实无死锁**：动画线程只调用 `render_handle.request_render()` → `Tui::request_render(false)` → `schedule_render`（仅 schedule 锁，短暂）→ 永不碰 `inner` 锁/pending 队列。因此 `stop()`/`Drop` 的 send+join 有界，不会与渲染循环互锁；`restart_animation` 先 `stop()` 再 spawn，多次 `start/set_indicator` 不会线程叠加。测试 `drop_stops_animation_thread_without_panicking`、`animates_frames_and_stop_freezes` 覆盖。
  - `cancellable_loader.rs`：AbortSignal 幂等、监听按注册序同步触发且脱离锁执行；`on_abort` 每次匹配键都触发（对应上游无条件下调用）；`\x1b`/`\x03` 经 `Keybinding::SelectCancel` 默认 escape+ctrl+c 匹配（keybindings.rs:1035-1037 测试佐证），无硬编码。
  - `recovery.rs`：panic hook 顺序 = 先 `restore_terminal` 再链上旧 hook 打印，对应上游 `ui.stop()` 先于 `console.error`；`try_stop` 用 `try_lock` + 毒锁恢复，锁被占时回退到固定序列 + `disable_raw_mode`（不 panic）；SIGTERM/SIGHUP → 恢复 → `exit(0)` 与上游 `shutdown({fromSignal:true})` 结束一致；非 unix 不注册（对应上游仅非 Windows 注册 SIGHUP）。恢复字节与 `ProcessTerminal::stop`/`stop_internal` 实际输出一致（`\x1b[?2031l`、`\x1b[?2004l`、`\x1b[<u`、`\x1b[>4;0m`、`\x1b[?25h`，terminal.rs:660-710、tui.rs:1859-1881）。
  - 11 个 snapshot golden 全部与上游语义手算一致（含 `finalizeTruncatedResult` 的 `prefix+reset+ellipsis+reset` 字节、`loader_frame0` 的 `\x1b[36m⠋\x1b[0m Loading…` 填充）。PIR_UPDATE_SNAPSHOTS 机制标准（写 golden 并 return，缺文件时给出再生成提示）。
  - 全部测试通过：443 lib + 11 snapshot + examples 构建。
- **Fixed**：无（只读审查，未改任何文件）。
- **Blocker**：无。
- **Note**：`Text`/`Box` 的 `RefCell` 缓存均为"持锁跨调用"检查过——`match_cache` 内部借用并在返回前释放，`render_text` 的读借用与后续 `borrow_mut` 无重叠；`render` 期间不持有缓存锁调用子组件。无双重借用/借用悬挂。

---

## 发现（按严重度）

### 中（Medium）

**M1. loader.rs:52,142 + tui.rs:988-991 — Arc 引用环：树内 Loader + `tui.render_handle()` 使 Tui 无法被 drop，动画线程永不 join（泄漏）**
- 问题描述：`Loader` 作为 `TuiInner.children` 的成员持有 `RenderHandle`；`Tui::render_handle()` 的闭包捕获 `tui.clone()`（tui.rs:990）。形成环：`TuiInner → children → Loader → RenderHandle → Tui clone → TuiInner`。若应用在 Loader 仍在树中、动画运行中 drop 掉最后一个外部 Tui 句柄（未先 `tui.clear()`/remove 组件），整个 Tui+组件树+Loader+动画线程永不释放——Rust 无 GC，上游 JS 场景由 GC 回收。线程每 80ms 继续对孤儿 Tui 调 `request_render`（schedule 锁，无害但空转）。当前示例进程退出时被系统回收，因此只在进程内重建/销毁 TUI 时暴露；D-016 未记录此环。
- 建议：① 在 `Tui::stop_internal`（或 `Tui::drop`）中清空 children 以断开环（注意 panic hook 路径持有 inner 锁，需确认 drop 子树安全——Loader::drop 的 join 不碰 inner 锁，安全）；② 或让 `RenderHandle` 闭包捕获 `Weak<Mutex<TuiInner>>` 并在 `upgrade` 失败时静默；③ 至少把"先 clear/stop 再 drop"写进 Loader 文档并加一个 drop 顺序测试。

**M2. recovery.rs:58-66 — 锁回退只覆盖"panic 线程自己持锁"场景；其他线程 panic 恰逢渲染持锁时终端仍可能残留 raw 态**
- 问题描述：若 panic 发生在工作线程（如 stdin 读线程、tokio 任务），而渲染线程恰在 `do_render` 中持有 inner 锁，`try_stop` 返回 false → 回退序列写入 stdout 并 `disable_raw_mode`；但渲染线程未 panic，会继续完成当前帧并向终端写出更多字节，`stopped` 标志也未置位——终端最终仍处于 raw 模式 + 隐藏光标（正是本模块要防止的状态）。上游单线程事件循环不可能出现"恢复与渲染并发"；Rust 多线程引入此窗口（渲染约每 16ms 一次、持锁亚毫秒级，窗口窄但存在）。文档只声明了"panic 线程持锁"一种回退场景。
- 建议：回退路径写完后，通过 `run_or_queue`（不阻塞）投递一个 `stopped=true` 的 op 让渲染停止；或在 `do_render` 开头检查一个 `restore_fired` 原子标志。若接受该残余窗口，建议在 docstring 中显式说明。

### 低（Low）

**L1. tests/snapshots.rs:150-156 — `loader_frame0` 存在时序竞态，golden 可能漂移**
- 问题描述：`Loader::new(None)` 用默认 80ms 间隔立即 spawn 动画线程；测试随即 `stop()`。若测试线程在 `new` 与 `stop` 之间被调度器抢占 >80ms 且动画线程恰好 tick（CI 高负载下可能），帧推进到 1，快照比对失败（flaky）；`PIR_UPDATE_SNAPSHOTS=1` 再生成时也可能录到非帧 0 的非确定 golden。
- 建议：改为显式构造超大间隔（如 `interval_ms: Some(60_000)`）的单帧/默认帧 Loader，使首 tick 不可能发生在 stop 之前；或先 `stop()` 再断言帧 0。

**L2. loader.rs:109-133 — `set_indicator` 中 `current_frame.store(0)` 在 join 旧线程之前**
- 问题描述：`set_indicator` 先 `store(0)`（130 行）再经 `start()→restart_animation()→stop()` join 旧线程（134 行起）。旧线程若恰在此微秒级窗口内 tick，会用旧 `frames_len` 把新索引写进 `current_frame`；`display_text` 对新帧表越界取 `""` → 瞬时一帧丢失指示符（或错帧）。自愈快，但可轻易消除。
- 建议：把 `current_frame.store(0)` 移到 `stop()`（join 完成）之后。

**L3. recovery.rs:82-99 — panic hook 不退出进程：工作线程 panic 后进程继续运行，与上游 exit(1) 语义不同（已文档化，提示级）**
- 问题描述：文档明确说明这是刻意偏离（Rust panic 继续 unwinding）。但后果是：非主线程 panic 恢复终端后进程继续跑，TUI 已 `stopped=true`（不再渲染），主循环若未感知会空转或误以为 TUI 正常。上游 `process.exit(1)` 无此问题。
- 建议：保持现状但确保 T12 交互模式的错误路径显式处理"hook 已恢复、TUI 已停"状态（如 panic 后由主循环检查退出），并在此文件注明该交接契约。

**L4. recovery.rs:140-231（测试）— 进程级 hook 无 RAII 恢复**
- 问题描述：`panic_hook_restores_terminal_before_chained_hook` 在断言全部通过后才 `std::panic::take_hook()`；若中途 assert 失败，自定义 hook 残留进程级，吞掉后续测试的 panic 输出、并持有一个活 Tui 引用（后续任意测试 panic 都会触发对其 `try_stop`）。
- 建议：用 guard 结构（Drop 里 `take_hook()`）或 `catch_unwind` 包裹断言段。

### 提示（Info）

**N1. recovery.rs:39-52 — 回退序列注释"same order"与真实 stop 顺序不完全一致**
- 真实 `stop_internal` 顺序是 `\x1b[?2031l` → 光标移动 → `\x1b[?25h`（show_cursor）→ `\x1b[?2004l` → `\x1b[<u` → `\x1b[>4;0m`；`MINIMAL_RESTORE_SEQUENCE` 把 `\x1b[?25h` 放在最后。这些模式切换相互独立、顺序无语义影响，仅注释措辞不精确。

**N2. recovery.rs:104-126 — 信号路径中回退序列实为"主路径"**
- 主线程常态在 `pump()` 持 inner 锁等待，SIGTERM 到来时 `try_stop` 大概率 WouldBlock → 回退序列 + `disable_raw_mode` 成为信号恢复的实际主路径；它不含进度 keepalive 清理与光标归位（文档已声明）。无动作，仅确认行为符合预期。

**N3. loader.rs:189-192 — 动画期间 Loader 内 Text 缓存几乎必然 miss（每帧文本变化），缓存只对 stop 后同帧渲染有效**。语义正确，性能可忽略，无动作。

**N4. text.rs:120-137 — `result.is_empty()→[""]` 为防御性死分支**（padding_y=0 时 content_lines 恒非空），与上游 `result.length > 0 ? result : [""]` 同为死代码，一致性 OK。

**N5. truncated_text.rs 模块注释"8 个用例"措辞含糊**：上游 truncated-text.test.ts 有 9 个 `it()`，Rust 侧 9 个测试全覆盖（8 同名 + 1 合并覆盖），覆盖完整，仅注释计数表述易误解。

---

## 结论
无 blocker；渲染字节与上游逐分支对齐（含 golden 复核）；Loader 线程生命周期（join/泄漏）经锁语义分析无死锁，唯一泄漏路径是 M1 的 Arc 环；recovery.rs 的"先恢复后输出"顺序正确，锁回退与信号路径行为符合文档，残余风险见 M2/L3。全部 443 lib + 11 snapshot 测试通过，examples 构建通过。

## Acceptance report