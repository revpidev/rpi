# T11：tmux.md / terminal-setup.md 字节序列对拍映射表

> 门禁 G3（逐条对拍级基准）：`external/pi/packages/coding-agent/docs/tmux.md` 与
> `external/pi/packages/coding-agent/docs/terminal-setup.md`（上游 2efa728 / v0.82.1）
> 的逐条映射——文档条目 → 字节序列（原样）→ rpi-tui 实现位置 → 测试锚点。
> 实现代码位于 `crates/rpi-tui/src/`，行号以本表制作时（2026-08-05）为准。

## 统计

- **tmux.md**：14 条；直接测试锚点 9、间接 2、无测试 3
- **terminal-setup.md**：17 条；直接测试锚点 9、间接 1、无测试 6、不适用（纯行为说明）1
- 合计 31 条；「无测试」条目均为**同一解析路径的变体**（CSI-u / modifyOtherKeys /
  legacy enter 分支），核心路径均有锚点；唯二无任何实现/锚点的功能面是
  macOS 原生修饰键检测与 Windows VT input（功能缺口，见 D-016）

---

## 一、tmux.md

| # | 文档条目（小节） | 字节序列（原样） | 行为要求 | rpi-tui 实现位置 | 测试锚点 |
|---|------------------|------------------|----------|------------------|----------|
| T1 | §Recommended Configuration | `\x1b[>7u\x1b[?u\x1b[c` | Kitty 键盘协议不可用时请求 extended key reporting（tmux `extended-keys on` 依赖此请求转发）；DA 应答无 Kitty flags 立即回退 modifyOtherKeys，无超时等待 | `terminal.rs` `ProcessTerminal::query_and_enable_kitty_protocol`（:357-366）、`parse_keyboard_protocol_negotiation_sequence`（:108） | `queries_kitty_mode_before_enabling_modify_other_keys_fallback`（terminal.rs:1149，断言首写 `\x1b[>7u\x1b[?u\x1b[c`）；`activates_kitty_mode_for_non_zero_negotiated_flags`（:1159）；`falls_back_to_modify_other_keys_for_zero_kitty_flags`（:1180）；`falls_back_to_modify_other_keys_for_device_attributes_without_kitty_flags`（:1206）；`parse_keyboard_protocol_negotiation_sequence_matches_upstream_regexes`（:1306） |
| T2 | §Why csi-u Is Recommended（xterm 格式） | `\x1b[27;5;99~` | Ctrl+C（tmux `extended-keys-format xterm` 转发，modifyOtherKeys 格式）解析为 `ctrl+c` | `keys.rs` `parse_modify_other_keys_sequence`（:840）、`matches_modify_other_keys`（:864），经 `matches_key`/`parse_key` 分发 | `matches_xterm_modify_other_keys_ctrl_c`（keys.rs:1902，含 `parse_key` 断言） |
| T3 | §Why csi-u（xterm 格式） | `\x1b[27;5;100~` | Ctrl+D 同上 | 同 T2 | `matches_xterm_modify_other_keys_ctrl_d`（keys.rs:1910） |
| T4 | §Why csi-u（xterm 格式） | `\x1b[27;5;13~` | Ctrl+Enter 同上 | 同 T2 | `matches_xterm_modify_other_keys_enter_variants`（keys.rs:1927，含 `\x1b[27;2;13~` shift、`\x1b[27;3;13~` alt） |
| T5 | §Why csi-u（csi-u 格式） | `\x1b[99;5u` | Ctrl+C（tmux 3.5+ `extended-keys-format csi-u`）解析为 `ctrl+c` | `keys.rs` `parse_csi_u_sequence`（:607）、`parse_kitty_sequence`（:750）、printable 分支（:1353-1364） | `matches_direct_codepoint_when_no_base_layout_key`（keys.rs:1767，直接断言 `\x1b[99;5u` → `ctrl+c`）；`matches_ctrl_c_when_pressing_cyrillic_s_with_base_layout_key`（:1733，`\x1b[1089::99;5u` 变体） |
| T6 | §Why csi-u（csi-u 格式） | `\x1b[100;5u` | Ctrl+D 同上 | 同 T5 | 间接：`matches_ctrl_d_when_pressing_cyrillic_v_with_base_layout_key`（keys.rs:1742，`\x1b[1074::100;5u` 同路径）；无 `\x1b[100;5u` 字面量断言 |
| T7 | §Why csi-u（csi-u 格式） | `\x1b[13;5u` | Ctrl+Enter 同上 | `keys.rs` enter 分支 `matches_kitty_sequence(data, CODEPOINT_ENTER, MOD_CTRL)`（:1127-1129） | 无直接测试（同路径锚点：`matches_super_modified_bindings_including_combined_modifiers` keys.rs:1775 断言 `\x1b[13;9u` = super+enter） |
| T8 | §What This Fixes 表格 | `\r` | Enter（无 extkeys 时 tmux 透传）解析为 `enter` | `keys.rs` enter 分支 `data == "\r"`（:1121）、`parse_key`（:1503） | `parses_special_keys`（keys.rs:2356，断言 `\r`→`enter`、`\t`→`tab`、`\n`→`enter`） |
| T9 | §What This Fixes 表格 | `\x1b[13;2u` | Shift+Enter（csi-u）解析为 `shift+enter` | `keys.rs` enter 分支 `matches_kitty_sequence(data, CODEPOINT_ENTER, MOD_SHIFT)`（:1085-1089） | 无直接测试（输入侧）；输出侧锚点：`rewrites_apple_terminal_return_to_csi_u_shift_enter_when_shift_pressed`（terminal.rs:1069）断言 `\x1b[13;2u` 生成 |
| T10 | §What This Fixes 表格 | `\x1b[13;3u` | Alt/Option+Enter（csi-u）解析为 `alt+enter` | `keys.rs` enter 分支 `matches_kitty_sequence(data, CODEPOINT_ENTER, MOD_ALT)`（:1102-1111） | 无直接测试 |
| T11 | §What This Fixes 表格 | `\x1b\r` | Alt+Enter legacy 形态（Kitty 未激活时）；Kitty 激活时同一字节按终端映射解释为 `shift+enter`（`\x1b\r` 与 `\n` 两条） | `keys.rs` :1115-1117（alt）、:1465-1469（kitty 激活 → `shift+enter`）、`parse_key`（:1528） | 间接：`treats_linefeed_as_shift_enter_when_kitty_active`（keys.rs:2065）覆盖同分支的 `\n`；`\x1b\r` 本身无字面量断言 |
| T12 | §What This Fixes（行为） | — | 默认键位：Enter submit、Shift+Enter newline；任何自定义 modified-Enter 键位依赖上述解析 | `keybindings.rs` 默认表 `tui.input.submit` = `["enter"]`（:527）、`tui.input.newLine` = `["shift+enter","ctrl+j"]`（:522-526） | `binds_ctrl_j_as_a_default_newline_alias`（keybindings.rs:783）；`does_not_evict_selector_confirm_when_input_submit_is_rebound`（:795） |
| T13 | §Requirements | — | tmux 3.2–3.4 无 `extended-keys-format`，Pi 仍支持其 xterm modifyOtherKeys 格式 | 同 T2–T4（modifyOtherKeys 解析不依赖 tmux 版本） | 同 T2–T4 |
| T14 | §Recommended Configuration（行为） | — | 探测无 Kitty 应答立即回退 modifyOtherKeys（无超时等待） | `terminal.rs` `query_and_enable_kitty_protocol` 回退路径 + `enable_modify_other_keys`（`\x1b[>4;2m`，:596）/`disable_modify_other_keys`（`\x1b[>4;0m`，:605） | `falls_back_to_modify_other_keys_for_zero_kitty_flags`（:1180，断言回退后写 `\x1b[>4;2m`）；`activates_kitty_mode_for_non_zero_negotiated_flags`（:1159，断言 kitty 激活时不写 `\x1b[>4;2m`/`\x1b[>4;0m`） |

---

## 二、terminal-setup.md

| # | 文档条目（小节） | 字节序列（原样） | 行为要求 | rpi-tui 实现位置 | 测试锚点 |
|---|------------------|------------------|----------|------------------|----------|
| S1 | §Kitty, iTerm2 | —（Kitty 键盘协议整体） | 开箱即用：协议协商启用 + CSI-u/flags 解析 | `terminal.rs` `query_and_enable_kitty_protocol`（:357）；`keys.rs` `parse_csi_u_sequence`（:607）、`parse_arrow_sequence`（:647）、`parse_functional_sequence`（:681）、`parse_home_end_sequence`（:720）、`is_key_release`/`is_key_repeat`（:505/:529） | 协商：T1 所列 5 个；解析：`matches_arrow_keys`（keys.rs:2216）、`parses_arrow_keys`（:2370）、`matches_legacy_function_keys_and_clear`（:2236）、`is_key_release_and_is_key_repeat_detect_event_suffixes`（:2410）等 |
| S2 | §Apple Terminal | `\x1b[>4;2m` / `\x1b[>4;0m` | 可用时启用 enhanced key reporting（modifyOtherKeys mode 2），停用/退出时关闭 | `terminal.rs` `enable_modify_other_keys`（:592-598）/`disable_modify_other_keys`（:601-607），stop 路径（:673-674, :721-724） | `queries_kitty_mode_before_enabling_modify_other_keys_fallback`（:1149，断言 `\x1b[>4;2m` 不先于探测出现）；`falls_back_to_modify_other_keys_for_zero_kitty_flags`（:1180）；`activates_kitty_mode_for_non_zero_negotiated_flags`（:1159，断言不写 `\x1b[>4;2m`） |
| S3 | §Apple Terminal | `\r` → `\x1b[13;2u` | 若 Terminal.app 仍对 Shift+Enter 发 plain Return，本地 shift 按下时归一化为 `\x1b[13;2u` | `terminal.rs` `normalize_apple_terminal_input`（:153-162）、`forward_input_sequence`（:577-589）、`is_apple_terminal_session`（:148） | `rewrites_apple_terminal_return_to_csi_u_shift_enter_when_shift_pressed`（:1069）；`leaves_apple_terminal_return_unchanged_when_shift_not_pressed`（:1077）；`leaves_non_apple_terminal_return_unchanged_when_shift_pressed`（:1082）；`leaves_non_return_input_unchanged`（:1087） |
| S4 | §Apple Terminal | —（本地 macOS 修饰键检测） | fallback 仅同机生效（远程 SSH 无法检测本地键盘） | `native_modifiers.rs` `is_native_modifier_pressed`（恒 `false`，:100 内） | 无测试——**功能缺口**：无原生绑定，macOS 上归一化永不触发（见 D-016 §功能缺口） |
| S5 | §Ghostty | `\x1b\x7f` | `alt+backspace` 映射目标（Ghostty 配置 `keybind = alt+backspace=text:\x1b\x7f`）解析为 `alt+backspace` | `keys.rs` backspace MOD_ALT 分支 `data == "\x1b\x7f" || data == "\x1b\x08"`（:1132-1138）、`parse_key`（:1534-1535） | 间接：`parses_legacy_alt_prefixed_sequences_when_kitty_inactive`（keys.rs:2165，断言同分支 `\x1b\x08`）；`\x1b\x7f` 本身无字面量断言 |
| S6 | §Ghostty | `\n` | `shift+enter=text:\n` 发送裸 linefeed；Kitty 激活时 `\n` 按 `shift+enter` 解释（与 Ctrl+J 不可区分），legacy 下按 `enter` | `keys.rs` :1096-1098（shift+enter）、:1121-1122（enter）、`parse_key`（:1466-1469, :1503） | `treats_linefeed_as_shift_enter_when_kitty_active`（keys.rs:2065）；`matches_legacy_linefeed_as_enter`（:2057）；`parses_special_keys`（:2361-2362） |
| S7 | §Ghostty | `ctrl+j` | Pi 绑定 Ctrl+J 为默认 newline alias，tmux 下 Shift+Enter 经此 remap 保持可用 | `keybindings.rs` 默认表 `tui.input.newLine` = `["shift+enter","ctrl+j"]`（:522-526） | `binds_ctrl_j_as_a_default_newline_alias`（keybindings.rs:783） |
| S8 | §WezTerm | `\x1b[13;3u` | Option+Enter（`wezterm.action.SendString('\x1b[13;3u')` 全屏覆盖）解析为 `alt+enter` | `keys.rs` enter 分支（同 T10） | 无直接测试（同 T10） |
| S9 | §WezTerm | `\x1b\x1b[27;…u` | `enable_kitty_keyboard` 下 Escape 按下为裸 `\x1b`、release 为 CSI-u，拼接为 `\x1b\x1b[27;…u`；缓冲层须拆分为独立 ESC + CSI-u，防 `\x1b\x1b` 被当作 meta 键 | `stdin_buffer.rs` `is_complete_sequence` 拆分逻辑（:282-306） | `split_esc_esc_csi_into_standalone_esc_and_csi_sequence`（stdin_buffer.rs:810）；`split_esc_esc_csi_with_no_modifier`（:823）；`still_emit_esc_esc_as_single_sequence_when_not_followed_by_new_escape`（:832） |
| S10 | §WezTerm（WSL IME）/ §IntelliJ IDEA | `RPI_HARDWARE_CURSOR=1`（上游 `PI_HARDWARE_CURSOR`，ADR-0001 改名） | 硬件光标可见（默认隐藏），IME 候选窗定位 | `tui.rs` `ENV_HARDWARE_CURSOR`（:530-531）、`showHardwareCursor` 路径（:722）、`CURSOR_MARKER`（:108）定位与剥离（:2426） | `show_hardware_cursor_makes_cursor_visible`（tui.rs:7190）；`cursor_marker_positions_hardware_cursor_and_is_stripped`（:7154） |
| S11 | §Alacritty | `\u001b[13;3u`（= `\x1b[13;3u`） | Option+Enter 重映射（macOS 下 Alacritty 发 plain Enter 的替代）解析为 `alt+enter` | `keys.rs` enter 分支（同 T10） | 无直接测试（同 T10） |
| S12 | §VS Code | `\u001b[13;2u`（= `\x1b[13;2u`） | 1.109.5 以下版本需显式 keybinding 发送 Shift+Enter 序列，解析为 `shift+enter` | `keys.rs` enter 分支（同 T9） | 无直接测试（同 T9） |
| S13 | §Windows Terminal | `\u001b[13;2u` / `\u001b[13;3u` | `sendInput` 转发 Shift+Enter / Alt+Enter 键弦 | `keys.rs` enter 分支（同 T9/T10） | 无直接测试（同 T9/T10） |
| S14 | §Windows Terminal | —（VT input） | `ENABLE_VIRTUAL_TERMINAL_INPUT` 使 Shift+Tab 等到达应用 | `terminal.rs`（:41-45 注释）：crossterm raw mode 不置该 flag、无原生 helper 绑定 | 无测试——**功能缺口**：Windows 上 Shift+Tab 仍为 `\t`（见 D-016 §功能缺口） |
| S15 | §xfce4-terminal, terminator | — | 行为说明：有限序列支持，修饰 Enter 不可区分 | 无实现需求（依赖终端侧能力） | —（不适用） |
| S16 | §IntelliJ IDEA | `RPI_HARDWARE_CURSOR=1` | 同 S10（硬件光标可见） | 同 S10 | 同 S10 |
| S17 | §Kitty, iTerm2（补充） | —（OSC 8 探测：`probeTmuxHyperlinks`） | 能力探测：tmux 客户端是否转发 OSC 8 超链接；tmux 下图像协议禁用 | `terminal_image.rs` `probe_tmux_hyperlinks`（:126，250ms 预算 + `try_wait` 轮询）、`detect_capabilities`（:177-191） | `test_detect_capabilities_enables_hyperlinks_under_tmux_when_client_forwards_them`（terminal_image.rs:981）；`test_detect_capabilities_disables_hyperlinks_under_tmux_when_client_does_not_forward`（:996）；`test_detect_capabilities_checks_tmux_capability_when_term_starts_with_tmux`（:1011） |

---

## 三、无测试条目汇总与说明

无直接字节级测试锚点的序列（均属同一解析路径的变体，核心路径有锚点）：

| 序列 | 文档条目 | 覆盖情况说明 |
|------|----------|--------------|
| `\x1b[13;2u`（输入侧） | T9 / S12 / S13 | CSI-u enter 分支：`matches_super_modified_bindings_including_combined_modifiers` 覆盖同函数同码位 `\x1b[13;9u`；输出侧有 terminal.rs:1069 |
| `\x1b[13;3u` | T10 / S8 / S11 / S13 | 同上（modifier 仅数值不同） |
| `\x1b[13;5u` | T7 | 同上 |
| `\x1b[100;5u` | T6 | `\x1b[1074::100;5u` 变体有断言（S6 同族） |
| `\x1b\r` | T11 | 同分支 `\n` 有断言（S6） |
| `\x1b\x7f` | S5 | 同分支 `\x1b\x08` 有断言 |
| macOS 原生修饰键检测 | S4 | **功能缺口**，无实现（恒 false），无测试 |
| Windows VT input | S14 | **功能缺口**，无实现，无测试 |

> 建议（可选跟进）：为 T7/T9/T10 补 `\x1b[13;2u`/`\x1b[13;3u`/`\x1b[13;5u` → `matches_key`/`parse_key` 的直接断言，成本低、可消除全部「无直接测试」标注。
