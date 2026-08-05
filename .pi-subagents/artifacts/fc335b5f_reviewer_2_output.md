# Review: crates/pir-tui/src/tui.rs（5201-7407 行测试模块 + Kitty 图像行算法 + 终端自省实现）

上游对照：`external/pi/packages/tui/src/tui.ts` @ `2efa728d2ee90ef597626e96b1e28ef2b279f07c`（已核对 `git rev-parse HEAD` 一致）。
说明：`/home/leven/develop/ai/pir/plan.md` 与 `progress.md` 不存在（ENOENT），改用 `docs/plan/v0.1/T11-pir-tui-core.md`、`UPSTREAM.md` 及上游源码核对。

**范围说明**：5201-7407 行全部位于 `#[cfg(test)] mod tests` 内。被审查特性（Kitty 图像行 expand/delete、OSC 11/?996n/16t 自省）的实现代码位于本范围之前的 tui.rs:457-520、910-948、1851-1856、1999-2071、2334-2512、2640-2953，已一并逐行对照上游 tui.ts:21-58、679-687、765-895、1099-1177、1288-1329、1335-1630 审查。

---

## 正确性核对（已逐行对照上游，无问题）

1. **Kitty 图像行算法与上游字节级一致**：
   - `parse_kitty_image_header`（tui.rs:477-502）↔ tui.ts:28-51：参数拆分、`i`/`r` 提取、0 与 >0xffffffff 拒绝、非整数拒绝，均一致。
   - `get_kitty_image_reserved_rows`（tui.rs:2367-2382）↔ tui.ts:1128-1140：`min(rows, maxIndex-index+1, lines.length-index)` 与空行/图像行截断逻辑一致；所有调用点（full_render、diff 路径、expand）的 `index <= max_index` 不变量成立，无 usize 下溢。
   - `expand_changed_range_for_kitty_images`（tui.rs:2386-2413）↔ tui.ts:1142-1163：条件 `i >= firstChanged || (i <= lastChanged && blockEnd >= firstChanged)` 逐字一致，previous_lines 与 new_lines 两侧都展开。
   - `delete_changed_kitty_images`（tui.rs:2416-2433）↔ tui.ts:1165-1177：Set 去重（Rust Vec+contains）与 `maxLine` 钳制一致；`first_changed..=max_line` 在 start>end 时为零迭代，不会 panic。
   - `full_render` 图像分支（tui.rs:2470-2512）↔ tui.ts:1301-1322：`\r\n`×N-1 + `\x1b[{N-1}A` + 放置序列 + `\x1b[{N-1}B`，`i += N`（TS 为 `i += N-1` + 循环 `i++`，等价）；`imageReservedRows <= height` 判定一致；`i += image_reserved_rows` 与 `continue` 组合正确跳过保留行。
   - diff 路径图像分支（tui.rs:2815-2851）↔ tui.ts:1453-1480：`\x1b[2K` + 逐行 `\r\n\x1b[2K` + `\x1b[{N-1}A` + 序列 + `\x1b[{N-1}B`，越界（`image_start_screen_row < 0 || +N > height`）回退全量重绘的条件一致；`render_end` 作为 `maxIndex` 传入与上游一致。
2. **终端自省与上游一致**：
   - `handle_input` 消费顺序（tui.rs:1886-1917）↔ tui.ts:765-795：OSC 11 → 配色方案 → input listeners → cell size → debug 键 → overlay 焦点恢复 → focused 组件，完全一致。
   - `consume_osc11_background_response`（tui.rs:2000-2020）↔ tui.ts:841-863：replies 计数守卫、pop_front、settled 检查、`send(rgb)`（None 即未解析成功）一致；超时后迟到的应答仍被 settled 查询消费（tui.rs:2010-2016 与测试 `osc11_keeps_consuming_a_late_reply_after_timeout` 对应）。
   - `consume_cell_size_response`（tui.rs:2046-2071）↔ tui.ts:877-895：`\x1b[6;h;w t` 严格匹配、0 值返回 true、`set_cell_dimensions` + invalidate + request_render 一致。
   - `terminal_colors.rs` 的 `parse_osc11_background_color`/`parse_terminal_color_scheme_report` 与 terminal-colors.ts 的 regex 语义一致（`[^\x07\x1b]*` + 终止符必须到串尾；`/i` 旗标由 `eq_ignore_ascii_case`/`is_ascii_hexdigit` 覆盖）。
   - `stop_internal`（tui.rs:1858-1882）↔ tui.ts:689-714 一致。
3. **测试覆盖真实性**：5201-7407 内 overlay 系列测试与上游 `overlay-non-capturing.test.ts` 的 46 个用例 1:1 对应（已逐个比对测试名，抽样 4 个逐行核对断言与流程）；OSC 11 五例（terminal-colors.test.ts:124-250）全移植；cell-size 两例对应 tui-cell-size-input.test.ts。断言均比对上游，未发现"声称覆盖但实际不断言"的情况。引擎级补充测试（16ms 节流、key release 过滤、CURSOR_MARKER 定位、行尾 SGR+OSC8 reset、listener 重写、debug 键、stop 光标）均有实质断言。
4. **验证命令结果**：
   - `cargo test -p pir-tui --lib`：443 passed / 0 failed。
   - `cargo test -p pir-tui`（含集成）：443 + 11 snapshots + 1 doc-test 全过。
   - `cargo clippy -p pir-tui --all-targets -- -D warnings`：通过，无警告。
   - `cargo fmt --all -- --check`：通过。

---

## 发现（按严重度）

### 高（Blocker）
无。

### 中（Medium）

**M1. `Tui::next_deadline()` 不含自省查询超时 deadline，超时依赖宿主循环持续调 `tick`**
- 位置：tui.rs:1053-1066（`next_deadline`）、tui.rs:3031-3046（`tick` 内超时触发）。
- 问题：上游用 `setTimeout` 保证查询超时独立于事件循环触发（tui.ts:1678-1686、1715）。本移植改为显式 deadline，但只在 `Tui::tick` 被调用且 `now >= deadline` 时触发。`next_deadline()` 只计算渲染节流与终端 flush deadline，不包含 `pending_osc11_background_queries`/`pending_terminal_color_scheme_queries` 的 deadline；若宿主循环按 `next_deadline()` 计算等待时长且空闲时得到 `None`（"无限等待"），遇到不应答的终端，`query_terminal_background_color`/`query_terminal_color_scheme` 的 oneshot 将一直不 resolve（上游 setTimeout 无此问题）。当前仓库唯一消费者 `examples/tui_smoke.rs:68` 恰好把等待钳制在 100ms 上限，掩盖了该问题；T12 交互循环必须纳入查询 deadline 或同样钳制等待。
- 建议：将最早的 pending 查询 deadline 并入 `next_deadline()` 的 `min` 计算；并考虑让 `has_pending_work()`（tui.rs:1071-1075）也反映未决查询，保持 `settle` 语义一致。

### 低（Low）

**L1. `parse_kitty_param_number` 比 JS `Number()` 严格，个别畸形参数解析结果不同**
- 位置：tui.rs:472-474（配合 tui.rs:477-502）。
- 问题：JS `Number("0x10")`=16、`Number("1e3")`=1000、`Number("5.0")`=5 且 `Number.isInteger` 均通过 → 上游接受；Rust `parse::<u64>` 全部拒绝。若某终端/组件发出这类参数（`i=0x10` 等），上游会提取该图像 id 参与 expand/delete，本实现会漏掉。
- 影响：极低——上游 `encode_kitty` 只输出纯十进制，正常数据流不受影响；仅影响畸形输入的对拍保真度。
- 建议：如追求严格对拍，可改用宽松解析（去空白后按 `u64`、十六进制、科学计数法分别尝试，再套 `0 < v <= 0xffffffff` 过滤）；否则在溯源注释中记录该差异。

**L2. 已 settle 的查询条目从不回收，队列无界增长**
- 位置：tui.rs:2010-2016（OSC 11 仅应答时 `pop_front`）、tui.rs:2032-2038 与 tui.rs:3040-3046（配色方案查询条目永不删除）。
- 问题：超时只置 `settled=true` 并 `sender.take()`，条目留在队列。重复发起查询且终端不应答时：OSC 11 队列每个超时条目占位直至有应答；`pending_terminal_color_scheme_queries` 无任何删除路径，线性增长。附带效应：每个超时的 OSC 11 查询令 `pending_osc11_background_replies` 永久 +1，之后任何形如 OSC 11 的输入会被静默吞掉。
- 说明：OSC 11 侧与上游完全一致（上游 settled 查询同样留队以吸收迟到应答，移植测试 `osc11_keeps_consuming_a_late_reply_after_timeout` 依赖此语义）；配色方案 Vec 是 Rust 侧特有（上游经 listener+unsubscribe 解析，无保留队列）。
- 建议：若做清理，须同时递减 `pending_osc11_background_replies`（否则会破坏迟到应答消费语义/对拍测试）；否则在注释中登记为上游继承的有意行为。

**L3. `query_terminal_color_scheme`（?996n）缺少 TUI 级测试**
- 位置：tui.rs:933-948（查询写 `\x1b[?996n` 与入队）、tui.rs:2023-2041（应答消费 + 全部未决查询 settle）、tui.rs:3040-3046（超时分支）。
- 问题：5201-7407 及全文件无任何测试调用 `query_terminal_color_scheme`——查询写入、pending 解析、超时均无断言；只有 terminal_colors.rs 的解析单元测试。上游同样无 TUI 级测试（parity 成立），但本移植把"所有未决查询收到报告即全部 resolve"（tui.rs:2032-2038）做成了 Rust 特有语义（上游经 listener 广播实现），值得一个测试锚定（含"报告先于超时"与"超时后报告到达"两条路径）。
- 建议：参照 `osc11_*` 五例补一个 `query_terminal_color_scheme` 的 write/resolve/timeout 测试。

### 提示（Note）

**N1. `settle()`/`has_pending_work()` 不感知查询 deadline**
- 位置：tui.rs:1071-1075、tui.rs:3566-3575。
- 说明：`settle` 在无 inbox/pending/render 时即返回，未决查询超时不阻止返回；测试通过手动 `tui.tick(now+5ms)` 驱动超时（tui.rs:7092），语义自洽。与 M1 同源，当前无测试受影响。

**N2. 崩溃路径双重恢复（与上游一致）**
- 位置：tui.rs:2868-2887（diff 渲染宽度溢出：先 `stop_internal()` 再 `panic!`）；panic hook（recovery.rs:58-89）随后 `try_stop` → 再次 `stop_internal()`，恢复字节写两遍。
- 说明：上游同路径为 `this.stop(); throw` 且 uncaughtCrash 再 `ui.stop()`（interactive-mode.ts:3624），属 parity；D-017 已登记恢复语义。无操作建议。

**N3. 查询 deadline 在 `run_or_queue` 排队前计算**
- 位置：tui.rs:915、938（`deadline = Instant::now() + timeout` 先于闭包入队）。
- 说明：若内锁被占、闭包延迟执行，有效超时窗口相应缩短。队列在 `tick` 内及时排空，实际影响可忽略。

**N4. plan.md/progress.md 缺失**
- 说明：任务要求读取 `/home/leven/develop/ai/pir/{plan,progress}.md`，两者均不存在；已以 `docs/plan/v0.1/T11-pir-tui-core.md`（验收记录、测试清单）与 UPSTREAM.md 为计划依据。仓库根目录确无 plan.md/progress.md（`find` 全仓无 progress 文件），建议补回或更新任务引用。

---

## 结论

未发现高严重度问题。Kitty 图像行 expand/delete 与 OSC 11/?996n/16t 自省的 Rust 移植与上游 tui.ts（2efa728）逐行对拍一致；测试覆盖真实（overlay 46/46、OSC 11 5/5、cell-size 2/2 与上游用例一一对应）；443+11+1 测试全过；clippy `-D warnings` 与 fmt 干净。主要遗留风险为查询超时的触发依赖宿主循环（M1）与查询队列回收（L2），均不影响当前仓库内消费者，但 T12 联调前应处理 M1。