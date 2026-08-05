# Review: crates/pir-tui/src/tui.rs (lines 1–2600, plus do_render remainder through ~3050)

**Scope reviewed** (per task): TUI core structures (lines 1–700), SharedComponent + re-entrancy queue (run_or_queue / try_read / drain_pending_ops), render pipeline `do_render` (2547–2960, incl. the differential half that runs past 2600), and the diff/render-scheduling machinery. Compared line-by-line against upstream `external/pi/packages/tui/src/tui.ts` @ commit 2efa728d. The `VirtualTerminal` test emulator (≥3075) is a test double, not engine code; its semantics are only cross-checked where the differential pipeline depends on it.

**Verification performed**: full read of tui.rs 1–3200; full read of upstream tui.ts; `cargo test -p pir-tui --lib` → 443 passed / 0 failed; lock-order analysis of all Mutex acquisitions; throttle-timing trace vs upstream `requestRender`/`scheduleRender`; diff-branch trace vs upstream `doRender`.

---

## What is correct (verified parity, with evidence)

- **差分算法** — line-by-line parity with upstream `doRender`: `firstChanged/lastChanged` scan, `appendedLines`/`appendStart` (tui.rs:2664–2686 vs tui.ts:1409–1429), deleted-lines branch with `compute_line_diff`, `clearStartOffset`, `moveBack` (tui.rs:2707–2763 vs tui.ts:1441–1496), scroll branch (`moveToBottom`, `scroll`, viewport/hardwareCursorRow updates), `renderEnd = min(lastChanged, newLines.length-1)`, kitty pre-clear scroll fallback, `finalCursorRow` + `previousViewportTop = max(prevViewportTop, finalCursorRow - height + 1)` (tui.rs:2942–2957 vs tui.ts:1597–1625). `compute_line_diff` (tui.rs:481–491) matches upstream `computeLineDiff` (tui.ts:1268–1272) exactly.
- **全量回退条件** — all full-render fallbacks match upstream: first render (tui.rs:2630), width change (2636), height change / Termux exemption (2648–2657), `clearOnShrink` + no overlays (2660–2668), `firstChanged < prevViewportTop` (2770–2778), deleted-lines `targetRow < prevViewportTop` / `extraLines > height` (2718–2740), kitty pre-clear scroll (2838–2848).
- **16ms 节流** — `schedule_render` (tui.rs:1249–1275) is timing-equivalent to upstream `requestRender`/`scheduleRender` (tui.ts:716–763): non-force coalesces via `requested`, delay = `max(0, 16 − elapsed)` (the `now < last` branch mirrors JS negative elapsed), `last_render_at` is set before `do_render` like upstream `lastRenderAt = performance.now()`. The only micro-difference: Rust measures elapsed at request time, upstream at nextTick time (<1ms; behaviorally identical).
- **锁顺序** — no ABBA: `next_deadline` (tui.rs:1053–1064) drops the `schedule` guard inside the block before taking `inner`; all other paths take `inner` → `schedule` / `inner` → component. `inbox`/`pending` guards are held only statement-locally, never nested with `inner`.
- **毒化恢复** — every lock site goes through `lock_shared`/`lock_component` (`into_inner` recovery), including `try_stop`.
- **Overlay state machine** — `set_focus_internal` (tui.rs:1301–1422) matches `setFocusInternal` (tui.ts:372–435) including the blocked/eligible restore transitions, `is_overlay_focus_ancestor` is cycle-safe with a visited set, `retarget_overlay_pre_focus`, `get_topmost_visible_overlay` focusOrder semantics, `resolve_overlay_layout` margin/percent/clamp math (`div_euclid` == JS `Math.floor` for negative numerators), `composite_overlays` working-height/`min_lines_needed`/viewportStart, and `composite_line_at` segment math (extract_segments parameter order verified against utils.ts:1138).
- **Engine helpers** — `parse_kitty_image_header`, `get_kitty_image_reserved_rows`, `expand_changed_range_for_kitty_images`, `delete_changed_kitty_images`, `extract_cursor_position`, `position_hardware_cursor`, `stop_internal` cursor restore, `consume_osc11_background_response`, `consume_cell_size_response` all match upstream semantics.

---

## Findings by severity

### 高 (High)

**H1. `stop()` 清除 render deadline，导致 stop/start 后 TUI 永久停止渲染（文件头声称的修复未实现，且产生死代码分支）**
- 位置: `crates/pir-tui/src/tui.rs:1862`（`stop_internal` 中 `lock_shared(&self.schedule).deadline = None;`），配合 tui.rs:1254–1257（`schedule_render` 在 `requested == true` 时直接 return、不设新 deadline）、tui.rs:3010–3014（`should_render` 要求 `deadline.is_some_and(...)` 才渲染）；文件头承诺在 tui.rs:59–61。
- 问题: 文件头明确声明 "the pending deadline survives `stop()` and fires on the first `tick` after `start()`"，且 tui.rs:3012–3014 的 `if self.stopped { false }` 分支正是为此设计（保留 requested+deadline、跳过渲染）。但 `stop_internal` 把 deadline 清成 `None`：① 该 stopped 分支成为死代码（stop 后 deadline 恒为 None）；② `requested` 保持 true；③ `start()` 里的 `request_render(false)` 走 `schedule_render` 早退，不会再设置 deadline。最终状态 = `requested=true, deadline=None`，`should_render` 永远不成立 —— 渲染永久停摆（直到某处调用 `request_render(true)`），且 `has_pending_work()`（tui.rs:1074）因 `schedule.requested` 恒为 true 而让事件循环持续空转。Loader 动画运行期间 `requested` 几乎总是 true，所以"有 spinner 时 stop 再 start"必然触发。
- 与上游对比: 上游 `stop()` 也是 `clearTimeout` + `renderRequested` 残留（tui.ts:689–714），重启后同样停摆 —— 本移植行为与上游 bug 一致，但**未兑现文件头承诺的修复**（行为等同上游泄漏，不是回归，是缺失修复 + 文档/代码矛盾）。
- 建议: 删除 tui.rs:1862 的 `deadline = None`（让 deadline 存活，重启后首个 tick 按文件头语义渲染），或在 `stop_internal` 中同时重置 `requested=false`。二者择一，并保留 3012 分支与文件头一致。

### 中 (Medium)

**M1. `Tui::pump` 在整个阻塞等待期间持有 inner 锁，与文件头声明直接矛盾**
- 位置: `crates/pir-tui/src/tui.rs:1104`（`let dispatched = self.lock_inner().terminal.pump(timeout);`）；文件头声明在 tui.rs:52–53（"the TUI lock is not held while the terminal waits for events"）。
- 问题: `MutexGuard` 存活整个 `terminal.pump` 调用，包括 `event_rx.recv_timeout(limit)`/`recv()` 阻塞等待（terminal.rs:900–935）。文件头说法不成立。影响: ① 等待窗口内（`timeout=None` 时无界）其他线程的 `Tui::stop()` 阻塞、`try_read` 类查询（`has_overlay`/`is_focused`）返回默认值；② 渲染 deadline 只能在 pump 返回后由 `tick` 触发 —— 当前消费者（tui_smoke.rs:68–72）靠 `next_deadline` 限时规避，但任何传入 `None`/长超时的消费者都会让动画/节流渲染延迟到下一个输入事件。
- 建议: 修正文件头措辞，或在 `Tui::pump` 中只对 dispatch 段加锁（先收事件再统一持锁处理）；至少为 `stop()`/`try_stop()` 的跨线程语义补充文档。

**M2. 在 inner 锁内执行用户回调时，回调内调用阻塞式公开方法会自死锁（仅 with_terminal 有警告）**
- 位置: 用户代码在持锁下执行的四个点: input listeners（tui.rs:1900–1911）、`visible` 闭包（tui.rs:1785–1792）、`on_debug`（tui.rs:1920–1922）、组件 `handle_input`/`render`（tui.rs:1994–1998 / 2961–2967）。阻塞式公开方法: `with_terminal`（887–894，唯一有文档警告）、`stop`（1020–1024）、`start`（945–950）、`next_deadline`（1053–1064）、`tick`（1080）、`pump`（1102）。
- 问题: input listener / visible 闭包是应用提供的闭包，若其中调用 `tui.stop()` 或 `tui.next_deadline()`（例如退出逻辑、超时查询），同一线程在已持有 inner 的情况下再 `lock_inner()` → 永久自死锁。上游 JS 是同步可重入的，此类调用合法。
- 建议: 为这些方法补充与 `with_terminal` 相同的 "never from within a callback" 文档，或提供 `try_stop` 类非阻塞变体作为公开 API。

**M3. 组件锁重入自死锁风险（契约未禁止）**
- 位置: `lock_component`（tui.rs:215–223）被 `handle_input`（1994–1998）、`render_children`（2961–2967）、`set_focus_internal`（1392–1412）、`invalidate`（1813–1821）、`contains_component`（1528–1538）在持有 inner 锁的同时调用。
- 问题: 若某组件在 `handle_input`/`render`/`shared_children` 内部持有自己的 `SharedComponent` 克隆并再次 `lock()` 自己（例如组件内部需要 `&mut` 状态的方法），将直接死锁。当前组件（Loader 等）只用 `RenderHandle` 规避，但冻结契约（文件头 12–16 行）并未禁止该模式。
- 建议: 在 `SharedComponent`/`Component` 的文档契约中明确 "组件回调内不得锁定自身 SharedComponent"（或在 `lock_component` 处加 `try_lock` + 重入检测）。

### 低 (Low)

**L1. `contains_component` 递归无环检测，shared_children 成环时栈溢出（abort）**
- 位置: tui.rs:1528–1538。
- 问题: 与上游 `containsComponent`（tui.ts:485–489）同为无保护递归，但 Rust 栈溢出直接 abort 进程（比 JS 的 RangeError 更严重）。`is_overlay_focus_ancestor`（1472–1496）有 visited 保护，此处没有。
- 建议: 加 visited 集合（复用 pointer-identity 模式）或在文档中声明 shared_children 必须无环。

**L2. `OverlayHandle::is_hidden` 在 overlay 移除后语义与上游不同**
- 位置: tui.rs:1197–1209。
- 问题: 上游闭包持有 entry 对象，移除后 `isHidden()` 仍返回 entry 最后状态；Rust 找不到 entry 时返回 false。若调用方在 `set_hidden(true)` 后 `hide()` 再查 `is_hidden()`，上游 true、Rust false。边缘场景，调用方通常不会这样做。

**L3. OSC11 已超时查询会吞掉后续查询的响应**
- 位置: tui.rs:2000–2019 + 3030–3036。
- 问题: 查询 A 超时后仍留在 `pending_osc11_background_queries` 队首（settled=true）；终端对后续查询 B 的响应到达时，`pop_front` 命中 A，`!query.settled` 为假 → 响应被吞，且 `pending_osc11_background_replies` 照常递减，B 只能等超时返回 `None`。上游逻辑完全相同（tui.ts:841–863 的 `shift()` + `!query.settled`），属上游继承 bug（parity），但可记录。

### 提示 (Note)

**N1. 查询队列条目永不清理（轻微内存增长）**
- tui.rs:3030–3045: 超时的 OSC11 查询永远留在 VecDeque；color-scheme 查询无论 settled 与否永远留在 Vec（上游用 listener + unsubscribe 无此问题，Rust 版是新增的轻微泄漏）。查询频率低、条目小，长期运行才可见。
- 建议: 超时/settle 时从队列移除条目。

**N2. `try_read` 竞争时返回默认值（has_overlay/is_focused 返回 false）与上游同步语义不同**
- tui.rs:734–740（文件头已文档化）。在 M1 的 pump 长等待窗口内该差异被放大。注意在组件 render/handle_input 内查询 `has_overlay()` 会得到错误答案 —— 有文档，属有意差异。

**N3. 并发/交错 show_overlay 时 z-order 可能相对调用顺序反转**
- tui.rs:802–822 + 1542–1574: entry id 在调用时分配，但 `focus_order` 在 op 实际执行时分配。单线程下 drain 紧跟 handle_input，顺序保持；仅当另一线程在排队与 drain 之间直接执行 show_overlay 时，后调用的 overlay 可能拿到更低 focus_order（渲染在底层、hideOverlay 弹出的对象也相反）。多线程语义本就不保证顺序，提示级。

**N4. `parse_kitty_param_number` 不接受 JS `Number()` 的十六进制写法**
- tui.rs:388–391: "0x10" 在 JS 中解析为 16，Rust `parse::<u64>` 失败。Kitty 协议实际使用十进制，影响可忽略。

**N5. 无 `handle_input` 实现的聚焦组件仍收到 no-op 调用 + 尾部 `request_render(false)`**
- tui.rs:1994–1998（文件头已文档化）。后果: 每次输入后多一次（通常无变化的）渲染与 16ms 节流重置；上游会完全跳过。

**N6. `do_render` 崩溃路径 panic 时同步输出模式未闭合**
- tui.rs:2872–2887: 宽度溢出路径在已写入 `\x1b[?2026h` 后 `stop_internal()` + `panic!`，未输出 `\x1b[?2026l`；`stop()` 也不负责闭合同步模式。上游同样（throw 前不闭合），parity，但恢复后终端可能残留同步输出模式。

---

## 未发现问题的重点领域（对照任务要求）

1. **差分算法边界** — `first_changed == previous_lines.len()` 的 appendStart、`first_changed >= new_lines.len()` 的删除分支、`compute_line_diff` 的视图换算、extra-lines 清理与 `moveBack` 数学均与上游逐字等价；未发现偏移/越界错误。
2. **全量回退条件** — 六类 fallback 条件与上游逐一对应，无遗漏、无多出的触发条件（`clear_on_shrink` 的 `overlay_stack.is_empty()` 限定与上游一致）。
3. **16ms 节流** — 语义等价（见上），唯一实质问题是被 H1 覆盖的 stop/start 停摆。
4. **Mutex 死锁面** — 未发现 ABBA；仅存在同线程重入自死锁（M2/M3）与 pump 长持锁（M1）两类风险。
5. **错误路径** — 崩溃日志/恢复/毒化处理齐全；OSC11/color-scheme 超时由 tick 驱动。

## 测试验证

`cargo test -p pir-tui --lib` → `443 passed; 0 failed`（0.08s）。现有测试未覆盖 H1 的 stop/start 挂起渲染场景（可在停止前构造 pending throttled render 后重启断言首帧渲染）。

---

## Review
- **Correct**: 差分渲染管线、全量回退条件、16ms 节流、overlay 布局/合成、焦点恢复状态机与上游 tui.ts 逐行等价；锁顺序无 ABBA；443 个测试全部通过。
- **Blocker**: 无（与上游行为等价的缺陷不构成移植回归，但 H1 是文件头承诺的缺失修复）。
- **Fixed**: 无（本次为只读审查，未修改任何文件）。
- **Note**: H1 需在合入前决策（按文件头语义保留 deadline 或重置 requested）；M1 的文件头声明与实现矛盾应修正。