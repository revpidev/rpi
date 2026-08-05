# Review: crates/pir-tui/src/tui.rs 第 2601-5200 行（overlay 栈 + focus 恢复状态机、输入 dispatch、render 主流程后半、终端自省查询）

范围说明：目标区间 2601-5200 覆盖 do_render 的后半段与全部 overlay/focus 相关测试；为完整核对 overlay 栈与 focus 恢复状态机、输入分发、节流/截止时间语义，我同时审查了区间外的相关引擎代码（1296-1812 状态机、1859-2073 停止与输入、2437-2960 render 主流程、1249-1275 调度、910-953 自省查询入口、996-1104 生命周期），并与上游 `external/pi/packages/tui/src/tui.ts`（submodule @ 2efa728d，已核对 `git submodule status`）逐段对照。运行 `cargo test -p pir-tui --lib`：**443 passed, 0 failed**。

结论：这是对上游的高保真移植。focus 恢复状态机（`set_focus_internal`/`resolve_blocked_overlay_focus_resume`/`get_visible_overlay_focus_restore`/`is_overlay_focus_ancestor`/`retarget_overlay_pre_focus`）、overlay push/pop（`show_overlay`/`overlay_hide`/`overlay_set_hidden`/`overlay_focus`/`overlay_unfocus`/`hide_overlay`）、`handle_input` 分发顺序、`do_render` 差分管线、CURSOR_MARKER 定位、16ms 节流均与上游语义一致；发现的偏离均为实现与文档意图不一致或事件循环层面的潜在问题，无高危阻断项。

---

## 发现（按严重度）

### 中

**M1. `stop()` 清除了渲染 deadline，与头部文档声明的 "deadline survives stop()" 矛盾 → stop()+start() 后渲染永久卡死（仅 force render 可恢复）**
- 位置：`crates/pir-tui/src/tui.rs:1861`（`stop_internal` 中 `lock_shared(&self.schedule).deadline = None;`）与 `tui.rs:1258`（`schedule_render` 中 `if schedule.requested { return; }`）及 `tui.rs:3009`（`tick` 的 `schedule.requested && schedule.deadline.is_some_and(...)`）
- 问题：三条路径组合后的实际行为是——`stop()` 把 `requested=true, deadline=None` 留在 schedule 里；`start()`（`tui.rs:1013`）调 `request_render(false)` → `schedule_render` 因 `requested` 仍为 true 而提前返回，不设置新 deadline；`tick` 的 should_render 要求 deadline 为 Some。结果：重启后 `requested=true`、`deadline=None` 永久成立，普通渲染永远不触发，只有 `request_render(true)`（force）能恢复。同时 `has_pending_work()`（`tui.rs:1071`）因 `requested` 恒为 true 而永远返回 true，以它做循环条件的调用方会空转。
- 与上游对照：上游同样在 stop 后泄漏 `renderRequested === true`（tui.ts:753 处 `if (this.stopped || !this.renderRequested) return`），头部注释（tui.rs:60-65）明确声称本移植修复了此问题（"the pending deadline survives stop() and fires on the first tick after start()"），`tick` 的 stopped 分支注释（tui.rs:3012-3014）也写了 "the deadline persists here"，但 `stop_internal` 却把 deadline 清掉了——实现自相矛盾，文档声明的修复并未生效，最终行为与上游 bug 相同。
- 建议：二选一：(a) `stop_internal` 不再清 deadline（仅置 `stopped=true`），让 tick 的 stopped 分支自然保留 deadline，重启后到期即渲染；(b) 保留清空，但在 `start()` 里改为：若 `requested && deadline.is_none()` 则按 `last_render_at` 重新计算 deadline。并补一个 stop+start 回归测试（当前 443 个测试无一覆盖 stop→start 序列）。

**M2. 自省查询超时 deadline 未纳入 `next_deadline`/`has_pending_work` → deadline 驱动的事件循环可能永远不触发超时**
- 位置：`crates/pir-tui/src/tui.rs:1053-1066`（`next_deadline` 只取 render deadline 与 `terminal.next_flush_deadline()`，不含 `pending_osc11_background_queries` / `pending_terminal_color_scheme_queries` 的 deadline）；对照 `tui.rs:3028-3046`（tick 中两个查询超时循环）。
- 问题：`query_terminal_background_color` / `query_terminal_color_scheme`（`tui.rs:910-953`）文档承诺超时后 resolve `None`，但超时只在 `tick(now)` 且 `now >= deadline` 时触发。若事件循环用 `pump(next_deadline())` 等待且当前既无 render 待办也无终端 flush deadline，等待时长会退化为调用方的兜底值（smoke 示例为 100ms 上限）；若循环使用 `pump(None)` 且终端无事件，查询将无限期挂起。当前仓库内唯一消费者 `examples/tui_smoke.rs:68-72` 以 100ms 封顶，问题尚属潜在；但对外暴露的 `Tui::next_deadline` 是文档化 API，语义不完整。
- 建议：`next_deadline` 增加两个 pending 查询队列的最小 deadline；`has_pending_work` 同理（或至少注明查询超时需要调用方周期 tick）。

### 低

**L1. `Tui::pump` 在阻塞等待终端事件期间持有 inner 锁；头部注释与实现不符**
- 位置：`crates/pir-tui/src/tui.rs:1102-1104`（`let dispatched = self.lock_inner().terminal.pump(timeout);`）
- 问题：临时 MutexGuard 存活至该语句结束，即 `terminal.pump` 的整个阻塞期（`timeout=None` 时为无限等待）。期间另一线程调用 `Tui::stop()` / `with_terminal()` 会阻塞直到有终端事件；`try_stop`（恢复路径）用 `TryLockError::WouldBlock` 规避了死锁，`run_or_queue`/`try_read` 也都非阻塞，所以不会死锁，但头部注释（tui.rs:56-59）"the TUI lock is not held while the terminal waits for events" 与实际不符（实际是终端回调不需要该锁，而非锁未被持有）。
- 建议：修正注释；若需消除持有，可在 pump 内先 `terminal.pump` 再 `lock_inner().tick`（需确认 terminal.pump 的 flush 路径不依赖 TuiInner）。至少将 `timeout=None` 的无限阻塞视为已知约束写入文档。

**L2. 聚焦组件为默认（无输入处理）实现时，每次输入仍触发一次节流渲染请求**
- 位置：`crates/pir-tui/src/tui.rs:1996-1997`（`lock_component(&focused).handle_input(data); self.request_render(false);`）
- 问题：上游仅在 `this.focusedComponent?.handleInput` 存在时才调用并 `requestRender`（tui.ts:834-838）；Rust 中 `Component::handle_input` 是带默认空实现的 trait 方法，恒为存在 → 聚焦组件不处理输入时也产生一次 16ms 节流渲染请求。头部注释已声明此差异（tui.rs:66-69），行为无害但会造成多余的 render 请求（节流后合并）。
- 建议：可保持现状（已文档化）；如要严格对齐，可给 trait 增加"是否实现"标记（如 `fn handles_input(&self) -> bool`），仅在 true 时 requestRender。

### 提示

**N1. `has_pending_work` 在 stop 后恒为 true（M1 的伴生现象）**——`tui.rs:1071-1077` 只看 `schedule.requested`，不检查 `stopped` 或 deadline 是否有效。若事件循环以 `while tui.has_pending_work() { pump(...) }` 收尾，stop 后无法退出。建议 `has_pending_work` 附加 `!stopped` 条件或改判 `requested && deadline.is_some()`。

**N2. CURSOR_MARKER 定位与上游完全一致（核对项，非问题）**——`extract_cursor_position`（`tui.rs:2437-2457`）自底向上扫描底部 `height` 行、剥离 marker、以 marker 前文本的 `visible_width` 为列，与 tui.ts:1238-1256 逐行等价；在 `apply_line_resets` 之前提取的顺序也与上游一致。`position_hardware_cursor`（`tui.rs:2971-3004`）的 clamp（行在 [0,total_lines-1]，列仅 ≥0）、空 buffer 不写、`show_hardware_cursor` 控制均与 tui.ts:1632-1663 一致。列不 clamp 到宽度上限与上游相同（终端自行截断），标记出现在视口上方时无法定位也与上游相同——均为上游语义的忠实保留。

**N3. overlay 合成排序、布局解析与上游一致**——`composite_overlays`（`tui.rs:2230-2334`）按 `focus_order` 升序合成、`working_height = max(result.len, term_height, min_lines_needed)`、`viewport_start`、宽度截断防护均与 tui.ts:1036-1095 一致；`resolve_overlay_layout`（`tui.rs:2075-2188`）的 margin/width/maxHeight/百分比（含负值与 NaN → 回退 center 的文档化差异）/offset/clamp 均与 tui.ts:901-1033 一致。`resolve_anchor_row/col` 用 `div_euclid` 精确匹配 JS `Math.floor` 对负分子的行为。

**N4. focus 恢复状态机的 id 化移植正确**——上游以 entry 对象身份比较（`restoreState.overlay === entry`），Rust 以唯一 `entry_id` 代替（`tui.rs:1429-1434`, `423-433`）；`set_focus_internal`（`tui.rs:1301-1422`）的四个分支（blocked 且 blocked_by==prev、blocked 但 blocked_by 卸载/有 FocusTarget、非 overlay 焦点转移建立 Blocked、set_focus(None) 的恢复/清理）与 tui.ts:372-435 语义逐条一致；`overlay_unfocus`（`tui.rs:1677-1741`）的 blocked 分支保留/改写 resume、非焦点但有 pending restore 的短路、fallback target 均与 tui.ts:555-585 一致。测试 5296-6494 覆盖了大部分状态机场景且全部通过。

**N5. 输入分发顺序与上游一致**——`handle_input`（`tui.rs:1885-1998`）的消费顺序（OSC11 → 颜色方案报告 → input listeners（consume/data 语义）→ cell size 响应 → shift+ctrl+d 调试键 → 聚焦 overlay 可见性重定向 → 焦点恢复（Eligible/Blocked）→ 按键释放过滤 → 派发 + requestRender）与 tui.ts:765-839 完全对齐；`is_key_release` 的 `:3` 后缀检测与 `\x1b[200~` 粘贴保护与 keys.ts:527+ 一致（keys.rs 本身不在本次行区间内，仅核对调用点语义）。OSC11 响应按 FIFO `pop_front` 匹配、超时后迟到回复仍被消费（测试 `osc11_keeps_consuming_a_late_reply_after_timeout` 验证）均与上游 `shift()` 一致。

**N6. 16ms 节流与 deadline 语义核对通过**——`schedule_render`（`tui.rs:1249-1275`）：force 置 `deadline=now`（上游 nextTick 立即渲染）；非 force 且已有请求时合并（上游 `if (this.renderRequested) return`）；延迟 = `16ms - (now - last_render_at)`（`saturating_sub` 对应 `Math.max(0, ...)`，负 elapsed 时 `16 + |elapsed|` 对应 JS 负 elapsed 的 16-(-x)）。渲染期间新请求的再调度：Rust 靠 deadline 持久化、下次 tick 触发，与上游 `if (this.renderRequested) scheduleRender()` 等价（上游注释 tui.ts:716-763 已核对）。tick 内 query 超时"无论 stopped 与否都触发"也与上游独立 setTimeout 一致。

**N7. 死锁/锁序核对通过**——锁序为 pending（临时）→ inner、schedule（临时）→ inner、inbox（临时）→ inner，不存在持 inner 再锁其他互斥量的路径；组件回调内的 TUI 变更走 `run_or_queue`（try_lock + pending 队列），`drain_pending_ops`（`tui.rs:778-793`）循环取快照、每轮释放 inner 后重取，无重入死锁；`lock_shared` 与 `try_stop` 均处理毒化/争用。`with_terminal` 阻塞锁的使用约束已在文档中声明。唯一注意点是 L1 的 pump 长持锁。

---

## 验证记录
- `git submodule status`：external/pi @ 2efa728d（与任务所述 commit 一致）。
- `cargo test -p pir-tui --lib`：443 passed; 0 failed（0.08s）。
- 逐段对照：tui.rs 1249-1275 ↔ tui.ts 700-763；1301-1422 ↔ 372-435；1472-1517 ↔ 462-479；1542-1812 ↔ 496-635；1859-1879 ↔ 689-714；1885-1998 ↔ 765-839；2000-2073 ↔ 841-895；2075-2334 ↔ 901-1095；2437-2459 ↔ 1238-1256；2547-2960 ↔ 1258-1625；2971-3004 ↔ 1632-1663；3006-3046 ↔ 1678-1715；910-953 ↔ 1670-1718。