# Review: pir-tui terminal.rs / stdin_buffer.rs / native_modifiers.rs (port of pi tui @ 2efa728d)

## 范围与对照

- 被审文件：`crates/pir-tui/src/terminal.rs`（1460 行）、`crates/pir-tui/src/stdin_buffer.rs`（1119 行）、`crates/pir-tui/src/native_modifiers.rs`（100 行）。
- 上游对照：`external/pi/packages/tui/src/terminal.ts`（531 行）、`stdin-buffer.ts`（434 行）、`native-modifiers.ts`（59 行）。`git rev-parse HEAD` = `2efa728d2ee90ef597626e96b1e28ef2b279f07c`，与任务指定 commit 一致。
- 任务说明引用的 `/home/leven/develop/ai/pir/plan.md`、`progress.md` **不存在**（仓库内无此两文件，仅有 `docs/plan/v0.1/` 下的 T 任务文档）。不影响审查，已按任务正文列出的文件与重点执行。
- 验证命令：`cargo test -p pir-tui` → 443 lib + 11 integration + 1 doc 全部通过；另用 /tmp 独立程序实测 Rust `from_utf8` 错误恢复与 Node/WHATWG decoder 的等价性、astral 去重分歧（见下）。

## 结论摘要

Kitty 协商状态机、DA 立即回退、StdinBuffer 跨 chunk 重组、bracketed paste、deadline/keepalive 语义均与上游**逐行吻合**（详见「正确」节）。未发现高严重度问题。发现 1 个中危（astral 字符去重与上游行为分歧，且头注释的论证有误）、若干低危/提示项（大多已在头注释中声明为有意差异或已知缺口）。

---

## 正确（有证据）

1. **Kitty 协商状态机与 DA 立即回退**（terminal.rs:348-366 `query_and_enable_kitty_protocol`、471-526 `route_data_sequence`/`read_keyboard_protocol_negotiation_sequence`、528-556 处理/缓冲/150ms flush、592-608 enable/disable modifyOtherKeys）：
   - 请求序列 `\x1b[>7u\x1b[?u\x1b[c` 与 `KITTY_KEYBOARD_PROTOCOL_QUERY` 逐字节一致（terminal.ts:17）。
   - `parse_keyboard_protocol_negotiation_sequence` 与上游两个正则 `/^\x1b\[\?(\d+)u$/`、`/^\x1b\[\?[\d;]*c$/` 语言等价（含 `\x1b[?c` 空数字串、`\x1b[?u` 不匹配、u32 饱和与 JS double 的 `flags !== 0` 观测等价）；`is_keyboard_protocol_negotiation_sequence_prefix` 等价。
   - 协商缓冲合并逻辑（buffer+sequence 先试解析→再试前缀→否则 flush 旧 buffer 后单独处理新 sequence）、150ms 定时器→显式 deadline（fire-once、process() 驱动重排）与上游一致；`tick` 中 StdinBuffer flush 结果回灌 `route_data_sequence` 与上游 stdinBuffer `data` 监听器路径一致。
   - DA 立即回退：`DeviceAttributes` 在 kitty 未激活时 `enable_modify_other_keys`（terminal.ts:246-249），无启动超时；先 DA 后 kitty 响应、先 kitty 后 DA 的两种到达顺序都能收敛到正确终态（kitty 激活会先 disable modifyOtherKeys）。测试 `queries_kitty_mode_before_enabling_modify_other_keys_fallback`、`falls_back_to_modify_other_keys_for_device_attributes_without_kitty_flags`、`tracks_split_kitty_confirmation`、`replays_buffered_csi_prefix_input_when_it_is_not_a_kitty_response` 均覆盖并通过。

2. **UTF-8 增量解码**（terminal.rs:1008-1039 `decode_utf8_incremental`）：用 /tmp 实测 Rust `from_utf8` 的 `valid_up_to`/`error_len` 在 overlong（C0 AF）、代理区（ED A0 80）、截断（E4 B8）、坏续字节（F0 9F 92 41）、>U+10FFFF（F5..）五类输入下的恢复结果，与 Node `setEncoding("utf8")`（WHATWG decoder）的输出**逐类一致**（每类均产生相同数量的 U+FFFD 且不回退已解码前缀）。跨 chunk 尾字节保持（第二个返回值）行为正确，测试 `decode_utf8_incremental_holds_incomplete_tail_and_replaces_invalid` 覆盖。

3. **stdin reader 线程与 channel**（terminal.rs:374-400）：单线程顺序读→增量解码→发送，UTF-8 字符不会跨 channel 消息拆分；EOF/读错误/send 失败三路退出；`stop()` drop receiver 后 send 必失败（mpsc 语义保证，无竞态窗口）；`handle_stdin_data` 在 `stdin_buffer` 为 None 时安全返回。SIGWINCH 转发器（unix，terminal.rs:410-426）用 `Handle::try_current()` 优雅降级。

4. **Bracketed paste 重组**（stdin_buffer.rs:398-470 `process`）：与上游逐段一致——`start_index>0` 时先抽取前置序列、丢弃不完整尾部（`_remainder` 丢弃与上游相同）、`paste_buffer` 持有内容直到 `\x1b[201~`、同一 chunk 内 start+end 的处理、remaining 递归、paste 内容含 `\x1b[200~` 不回扫；`handle_stdin_data` 用 `\x1b[200~...\x1b[201~` 重包裹（terminal.ts:195-199）。上游全部 paste 测试均镜像并通过（含跨 chunk、Unicode、前后混合）。

5. **deadline/keepalive 语义**：10ms StdinBuffer flush（`process()` 先清 deadline、尾部残留才重排；`flush_expired` 仅到期触发一次）、150ms 协商片段 flush（fire-once）、1s OSC 9;4 keepalive（`set_progress(true)` 只排一次、`tick` 到期重写并重排、`set_progress(false)`/`stop()` 清除并只在曾有 keepalive 时写 CLEAR——与上游 `clearProgressInterval()` 布尔返回一致）；`drain_input` 的 `timeLeft<=0` 先于 `idle` 判定、同一 `now` 计算两个条件，与上游循环顺序一致；`max=0`/`idle>max` 边界行为一致。测试 `set_progress_writes_active_keepalive_and_stop_clears`、`set_progress_false_writes_clear_without_keepalive`、`drain_input_disables_kitty_protocol_and_restores_input_handler` 覆盖。

6. **native_modifiers.rs**：`load_native_modifiers_helper` 保留上游 darwin+x64/arm64 门控、永远返回 None 的行为与上游 helper 缺失路径一致（native-modifiers.ts:51-58），不会 panic。

---

## 发现（按严重度）

### 中

**M1. stdin_buffer.rs:556-566（`emit_data_sequence`）+ 头注释 14-18 — astral 字符去重与上游行为分歧，且头注释论证错误**
- 问题：上游 `rawCodepoint = sequence.length === 1 ? sequence.codePointAt(0) : undefined`（JS `length` 是 UTF-16 单元数）。astral 字符（如 `😀` U+1F600）在 JS 中 length=2 → `rawCodepoint = undefined` → **永远不会**与 pending 比较、永远不会被去重丢弃。端口用 `sequence.chars().count() == 1`（码点数），astral 字符计 1 → `raw_codepoint = Some(0x1F600)`。当 pending 为 `Some(0x1F600)`（由前一序列 `\x1b[128512u` 设置，`parse_unmodified_kitty_printable_codepoint` 允许任意 `>=32` 码点）时，端口会把紧随的原始 `😀` 当重复字符**静默丢弃**，而上游会正常发出。已用独立程序复现：`\x1b[128512u😀` 上游产出 `["\x1b[128512u","😀"]`，端口产出 `["\x1b[128512u"]`；BMP 情形（`\x1b[224uà`）两者一致（都丢弃）。
- 头注释（stdin_buffer.rs:14-18）声称 "upstream would compare a lone surrogate half (which could never equal a pending codepoint)"——这有两处错误：(a) 上游对 astral 字符根本不比较（length=2 → undefined），不存在"lone surrogate half"参与比较；(b) 端口的实现反而**会**比较（完整码点 vs pending，而 pending 可以是 astral 码点）。即注释描述的实现意图与上游不符，且实际行为与上游相反。
- 建议：与上游对齐，按 UTF-16 单元数判定：`sequence.encode_utf16().count() == 1`（BMP 单字符行为不变，astral 字符永不参与去重）。或至少在头注释中修正论证并补充 astral 用例测试。
- 触发面：需要"终端同时发送 CSI-u 按键与原始字符"（去重特性针对的真实场景，上游用 `à` 测试）+ 该键产生 astral 码点（当前键盘布局/终端罕见，故未列高）。真实但窄。

### 低

**L1. terminal.rs:374-400 + 660-710 — `stop()` 后 reader 线程仍阻塞在 `read()` 并持有进程级 stdin 锁**
- 问题：`stop()` 无 `process.stdin.pause()` 等价物（头注释已声明）。reader 线程在 `io::stdin().lock()` 的 `read()` 上阻塞，持有全局 StdinLock；此后任何对 stdin 的再次读取（包括 start-stop-start 重启场景下新 reader 的 `lock()`）都会阻塞直到下一个输入字节到达。上游 pause() 后重启是正常支持流程，端口不支持。
- 建议：在 `stop()` 中无法可靠中断阻塞读；可考虑 (a) 记录"已停止"状态，重启时复用/重启线程前先 drain；(b) 文档明确禁止 start-stop-start 序列（当前代码库无此路径，recovery 只 stop 不重启，故仅列低危）。

**L2. terminal.rs:712-760 — `drain_input` 在 tokio runtime 之外 await 会 panic**
- 问题：`tokio::time::sleep(...).await` 无 runtime 时 panic（"there is no reactor/timer running"）。trait 签名（terminal.rs:188-193）未体现该前提；测试双例（tui.rs:3497 `VirtualTerminal::drain_input`）是无操作实现，掩盖了该约束。T12 interactive mode 调用时若在 shutdown 路径（runtime 已 drop）调用即 panic。
- 建议：调用前断言/文档化 runtime 前提；或改用 `std::thread::sleep` + 非阻塞轮询（在 async 中会阻塞 executor，不推荐）；或在文档注明必须在 runtime 内 await。

### 提示

**N1. stdin_buffer.rs:14-18 头注释** — 见 M1，论证与实现均需修正（与 M1 合并处理）。

**N2. terminal.rs:450-469 — drain 期间丢弃的事件不经过协商处理（与上游不同）**：上游 `drainInput` 期间 stdinDataHandler 仍在，迟到的 DA/kitty 响应仍会走 `handleKeyboardProtocolNegotiationSequence`（可能写出 `\x1b[>4;2m`）；端口直接丢弃 channel 事件。进程随后退出，行为无害，但注释"Both leave the drained bytes unhandled"不准确（上游只不转发到 inputHandler，协商仍处理）。

**N3. terminal.rs:738-744 — `drain_input` 把 Resize 事件也计入 `last_data_time`**：上游 onData 只对 `data` 事件打时间戳；resize 风暴会延长 drain。极边缘。

**N4. terminal.rs:712-760 — `drain_input` future 被 drop 时 `input_handler` 不恢复**：Rust async 块被 drop 即中止，末尾 `self.input_handler = previous_handler` 不会执行（上游 try/finally 在 abort 时也会恢复）。当前无调用方，T12 需注意。

**N5. examples/tui_smoke.rs:29-63 — `tui.start()` 在 runtime 上下文之外调用，SIGWINCH 转发被静默禁用**：`rt.enter()` 只在 `spawn_signal_restore` 闭包内生效（tui_smoke.rs:31），`tui.start()`（:63）调用时 `Handle::try_current()` 失败 → `spawn_resize_forwarder`（terminal.rs:410-426）不注册 → 示例中 resize 不会触发重渲染。终端交互模式（T12）在 runtime 内 start 则无此问题；示例可把 `tui.start()` 移入 `rt.enter()` 作用域。

**N6. terminal.rs:389 / 900-931 — 无界 mpsc channel 无背压**：reader 线程按终端速度持续入队，若事件循环长时间不 `pump`（例如被长同步操作占住）内存无界增长。上游 Node 流有 highWaterMark/pause 背压。当前 pump 由事件循环持续驱动，风险低。

**N7. terminal.rs:900-931 — `pump(None)` 的两种无事件源分支行为不一致**：`(None, None)` 立即返回，`(Some(rx), None)` 永久阻塞（reader 线程存活且无输入时，`recv()` 不返回——EOF 时 sender drop 后 `recv()` 返回 Err，不会死锁）。调用方（tui_smoke、未来 T12）均传 `Some(timeout)`，无实际影响。

**N8. native_modifiers.rs — Apple Terminal Shift+Enter 归一化永远不触发**：`is_native_modifier_pressed` 恒为 false（无原生绑定），`forward_input_sequence` 中的归一化路径（terminal.rs:578-590）在 macOS 上不可达。头注释已声明，属已知缺口（与上游 helper 缺失行为一致）。

---

## 残余风险

- `drain_input` 尚无生产调用方（仅测试）；runtime 前提、drop 不恢复 handler 等约束未在真实路径验证。
- start-stop-start 重启序列不受支持（L1）；恢复路径（recovery.rs）只 stop。
- Windows `ENABLE_VIRTUAL_TERMINAL_INPUT` 缺失（Shift+Tab 退化为 `\t`）、resize 事件未接线——均为头注释声明的有意缺口。
- 协商状态机存在与上游相同的固有限制：用户在 10ms 内快速输入 `\x1b[?7u` 会被当作 kitty 响应激活协议（上游同构，非移植缺陷）。
- 上游测试套件中与本次范围相关的用例均已镜像；astral 去重差异（M1）无测试钉住。

## 验证记录

- `cargo test -p pir-tui`：443 lib + 11 integration + 1 doc 全通过。
- /tmp/utf8_check.rs：实测 Rust `from_utf8` 五类非法输入错误恢复，与 Node/WHATWG decoder 输出等价。
- /tmp/dedup_check.rs：复现 astral 去重分歧（port_drops=true, upstream_drops=false）与 BMP 一致性。