# Review: crates/pir-tui/src/keys.rs + keybindings.rs vs upstream (external/pi @ 2efa728d)

Scope: `keys.rs` (2445 行, Kitty flags=7 解析 + legacy CSI 全表) and `keybindings.rs` (1040 行, 31 条键位 + JSON 配置), both ports of `external/pi/packages/tui/src/{keys,keybindings}.ts`. Read-only review; no files modified.

Note: `plan.md` / `progress.md` referenced by the task do not exist in the repo (verified — root only has README/UPSTREAM/docs; deviations are documented in `docs/plan/v0.1/deviations/D-016-pir-tui-core-rust-notes.md`, which covers keys.rs/keybindings.rs).

## Correct (verified, with evidence)

- **Legacy CSI 表全部字节级一致**：`LEGACY_KEY_SEQUENCES`(23 键)、`LEGACY_SHIFT_SEQUENCES`(11)、`LEGACY_CTRL_SEQUENCES`(11)、`LEGACY_SEQUENCE_KEY_IDS`(58 条) 与上游逐条机械比对（脚本提取 + 归一化），0 差异。见 keys.rs:165-330。
- **Kitty 常量**：MOD_SHIFT/ALT/CTRL/SUPER=1/2/4/8、LOCK_MASK=64+128、CODEPOINT 集、ARROW(-1..-4)、FUNCTIONAL(-10..-15)、`KITTY_FUNCTIONAL_KEY_EQUIVALENTS`(27 项) 全部与上游一致（keys.rs:55-64, 88-144）。
- **`parse_csi_u_sequence` 手写游标与上游正则语义等价**：对 `\x1b[99:u`、`\x1b[99::u`（双双失败）、`\x1b[99:::5u`（失败）、`\x1b[99;:3u`（事件=3, mod=0）、`\x1b[2;:3~`（同上）、`\x1b[99;5::3u`（失败）等刁钻输入逐一推演，游标顺序（`:shifted` → `:base` → `;mod` → `:event`）与正则组的贪婪/回溯结果一致（keys.rs:607-664）。
- **`parse_kitty_sequence` 分派顺序**（CSI-u → arrow → functional → home/end）与上游相同；未知 keyNum 返回 None 对应上游 `funcCodes[keyNum]` undefined 的 fall-through（keys.rs:760-785）。
- **`matches_key` 全部分支**（escape/space/tab/enter/backspace/insert/delete/clear/home/end/pageUp/pageDown/up/down/left/right/f1-f12/单字符+修饰）与上游逐分支比对一致，包括 kitty 模式开关对 `\n`、`\x1b\r`、`\x1bB`、`\x1bF`、`\x1b `、alt+ 前缀的 gating 逻辑（keys.rs:1024-1330）。
- **`parse_key` 分支顺序与内容**与上游完全一致（含 kitty-active 时 `\x1b\r`/`\n`→shift+enter 的优先级、`\x1bOe`→ctrl+clear 等 58 条 legacy 表、末尾 raw ctrl+letter 区间 1..=26 / 32..=126）（keys.rs:1448-1600）。
- **`decode_kitty_printable`/`decode_modify_other_keys_printable`/`decode_printable_key`** 与上游一致（shift 优先取 shifted key、允许掩码 SHIFT|LOCK、<32 拒绝）（keys.rs:1612-1665）。
- **`is_key_release`/`is_key_repeat`** 与上游逐字节相同（含 `\x1b[200~` paste 防护）（keys.rs:349-394）。
- **keybindings 默认表 31 条**（id、keys、description）与上游 `TUI_KEYBINDINGS` 机械比对：31/31 全部一致（keybindings.rs:471-558）。
- **`rebuild` 冲突检测语义**：user-claims 首见序（对应上游 Map 迭代序）、claimants 去重保序（对应上游 Set 迭代序）、unknown id 忽略、user 覆盖替换 defaults、`get_resolved_bindings` 单键 Single/其余 Multiple —— 与上游一致（keybindings.rs:646-702, 631-644）。
- **JSON 保序**：`KeybindingsConfig` 自实现 `Serialize`/`Deserialize`（`serialize_map` + 自定义 visitor），insert 覆盖已存在 id 保持原位置（JS 对象语义），往返保序（keybindings.rs:345-459）。
- **测试**：`cargo test -p pir-tui --lib` 443 通过（keys 69 + keybindings 16 在其中），含 Cyrillic base-layout、Dvorak 反例、modifyOtherKeys、WT_SESSION/SSH 环境分支、null 解绑、冲突检测等用例。

## 按严重度列出的发现

### 中

**1. keybindings.rs:723-735 — `set_keybindings` 首装生效（OnceLock）与上游"后装覆盖"语义不同，且文档依据与上游源码不符**
`GLOBAL_KEYBINDINGS` 是 `OnceLock<RwLock<...>>`，`set_keybindings` 只在首次成功（`let _ = ...set(...)`），后续安装被静默丢弃。文档声称"Every upstream flow installs before any component reads the singleton and re-installs reuse the same instance, so the observable behaviour is identical"——但"re-installs reuse the same instance"不成立：上游三处调用点各自新建实例（`startup-ui.ts:81`、`session-picker.ts:23`、`interactive-mode.ts:468-469` 均为 `setKeybindings(KeybindingsManager.create())`，`create()` 每次 `loadFromFile` 读配置新建 manager），语义是**最后一次安装生效**。Rust 是**第一次安装生效**，之后的安装全部被忽略。后果：多安装流程（startup → session-picker → interactive）中若两次安装的配置不同（例如用户在 session-picker 等待期间编辑 keybindings.json，上游下次安装会读到新配置、Rust 不会）；嵌入方/测试也无法热切换全局注册表。
建议：改为"可替换"语义（如 `OnceLock<RwLock<OnceLock<...>>>` 或在 `set_keybindings` 中写入并暴露一个可写槽位），或至少在文档中如实描述差异并补一个"重复安装被忽略"的测试固化该行为。当前树内无生产调用方（见发现 5），风险暂为潜伏。

### 低

**2. keys.rs:582-595 + 1612-1659 — i32 溢出解析为 None 与 JS `parseInt` 的浮点行为存在一处可观察分歧（decode_kitty_printable）**
`Cursor::parse_digits` 对溢出 i32 的数字串返回 None。文件头注释声称与上游观察一致——对 `matches_key`/`parse_key` 路径推演确实如此（上游溢出后 normalize/比较全部落空，最终同样 false/undefined）。但 `decode_kitty_printable` 存在反例：输入 `\x1b[97:99999999999999999999;2u`（shift 按下、shifted 字段溢出）——
- 上游：`shiftedKey = 1e20`（number），`effectiveCodepoint = 1e20`，`String.fromCodePoint(1e20)` 抛 RangeError → `undefined`；
- Rust：shifted 解析失败为 None，退回 codepoint=97 → 返回 `'a'`。
终端绝不会发出溢出 i32 的 shifted key（单个码点），故实际不可达；但注释的"观察等价"表述过强。建议：将注释限定为"终端可达输入"，或对 shifted 溢出同样返回 None 使两边一致。

**3. keybindings.rs:390-396 — JSON `null` 解绑语义为有意的偏离，但改变了"崩溃"为"静默失效"**
`visit_unit` 将 `null` 映射为 `Multiple([])`（解绑）。上游把 `null` 原样存入并在 `matches` 时 TypeError 崩溃。偏离已文档化且更健壮，但注意：`"tui.input.submit": null` 从"用户配置错误导致崩溃（可被察觉）"变成"动作被静默禁用（难察觉）"。若 T12 的配置写入/校验环节不阻止 null，用户会以为是 bug。建议：在配置加载层（T09/crates/pir/src/core/keybindings.rs）对 null 给出 warning 或报错，而不仅是静默解绑。

### 提示

**4. keys.rs:829, 1410 — `char::from_u32` 与上游 `String.fromCharCode`（16 位截断）在符号判定上的理论分歧**
上游 `SYMBOL_KEYS.has(String.fromCharCode(cp))` 对 >0xFFFF 的码点取低 16 位；Rust `char::from_u32(cp as u32)` 用完整码点。对低 16 位恰为 ASCII 符号的码点（如 0x1002F → 上游视为 '/'），base-layout 回退判定会不同。真实终端路径（KP 键归一化后 ≤0xFFFF，非拉丁码点均为 BMP 字符）不可达。无需修改，可在注释中说明。

**5. keybindings.rs:733 — 当前树内 `set_keybindings` 无生产调用方，用户 keybindings.json 覆盖不会进入 pir-tui 全局注册表**
全仓 grep：`set_keybindings` 仅在本文件及测试中出现；组件侧只有 `cancellable_loader.rs:129` 调 `get_keybindings()`（永远返回默认 31 条表）。T09 的 `crates/pir/src/core/keybindings.rs` 明确注释"TUI runtime (T12) owns lifecycle"。属 T12 接线缺口而非本文件缺陷，提请父会话注意：在接线前，所有组件键位行为都是默认表，用户配置不生效。

## 结论
未发现高严重度问题。这是一个忠实度非常高的移植：四张 legacy 表与默认键位表字节级一致，CSI-u/箭头/功能键/Home-End 的游标解析在包括病态输入在内的推演中与上游正则语义等价，测试充分（443 通过）。主要风险集中在全局单例的"首装生效"语义（中）与溢出解析的一个理论反例（低）。

## 验证记录
- `git -C external/pi rev-parse HEAD` → 2efa728d（上游基准确认）
- 脚本比对：LEGACY 3 表 + LEGACY_SEQUENCE_KEY_IDS(58) + KITTY_FUNCTIONAL(27) + TUI_KEYBINDINGS(31) 全部 0 差异
- `cargo test -p pir-tui --lib` → 443 passed; 0 failed
- 上游 `setKeybindings` 调用点核查：startup-ui.ts:81 / session-picker.ts:23 / interactive-mode.ts:468-469（均新建实例，证实"reuse same instance"表述有误）