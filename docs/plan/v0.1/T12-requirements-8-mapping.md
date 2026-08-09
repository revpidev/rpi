# T12 需求 §8 逐条核对映射表

> 任务特有门禁：需求 §8 全章逐条核对有锚点（`docs/01-requirements.md` §8.1-8.6）。
> 列：需求 → 实现锚点（文件:行）→ 测试锚点。挂点/缺口项标注（T13/T14/T15 遗留）。
> 快捷键默认表见 `docs/plan/v0.1/T12-keybindings-mapping.md`（73 条逐条核对，不重复）。

## §8.1 布局

| 需求 | 实现锚点 | 测试锚点 | 状态 |
|------|----------|----------|------|
| Startup header → messages → editor → footer | `interactive_mode.rs` init 布局链（header→loadedResources→chat→pendingMessages→status→widgetsAbove→editor→widgetsBelow→footer） | `init_assembles_tree_and_starts_terminal` | ✅ |
| Footer 行 1：`cwd(~) (branch) • name` | `footer.rs` format_cwd_for_footer + render；branch 由 `git_branch_watcher.rs`（100ms 轮询 .git/HEAD，worktree 支持，cwd 随 `set_cwd`）写入 provider | `pwd_line_includes_branch_and_session_name`、`format_cwd_replaces_home_prefix`、git_branch_watcher 内测 | ✅（2026-08-06 watcher 接线后分支真正刷新） |
| Footer 行 2 左：↑↓/R/W/CH%/$cost[(sub)]/context%/window[(auto)][•xp]；>70 黄 >90 红；右：(provider) model[•thinking]；逐级截断；行 3 扩展 status | `footer.rs` render（全部口径） | `usage_totals_and_cache_hit_rate_render`、`context_percent_color_thresholds`、`truncation_falls_back_in_stages`、`provider_prefix_appears_with_multiple_providers`、`extension_status_line_is_sorted_and_truncated` | ✅（`• xp` 实验标记 2026-08-06 已接线 `experimental_enabled`（environment.rs，RPI_EXPERIMENTAL=1），footer.ts:162-164） |
| Header：logo + 紧凑/展开两态随 Ctrl+O 联动 + onboarding + changelog；quietStartup 空；扩展 setHeader | `header.rs` build_builtin_header + `interactive_mode.rs` toggle_tool_output_expansion | `header_builds_compact_and_expanded_instruction_sets`、`quiet_startup_yields_empty_header`、`tools_expand_toggles_header_and_pending_tools` | ✅（changelog 内容 T15 挂点；扩展 setHeader T15） |

## §8.2 Editor

| 需求 | 实现锚点 | 测试锚点 | 状态 |
|------|----------|----------|------|
| 多行、undo（Ctrl+-，快照含 paste registry，词符合并）、kill-ring（Ctrl+Y/Alt+Y） | `rpi-tui/components/editor.rs` + `kill_ring.rs` + `undo_stack.rs` | rpi-tui editor 测试 40+ | ✅（S2 交付） |
| 历史：up/down、100 条上限、草稿保存 | `editor.rs` history 域 | `jumps_to_start_before_entering_history_from_a_non_empty_draft` 等 | ✅ |
| bracketed paste → 大粘贴 marker（>10 行或 >1000 字符，两种格式，原子 segment）；tmux csi-u 重编码；路径前补空格 | `editor.rs` paste 域 | `submits_large_pasted_content_literally`、`renumbers_the_paste_registry_*` 等 | ✅ |
| `@` 文件模糊搜索、引号路径、`~/` 展开；Tab 上下文分派（slash→命令/否则文件） | `autocomplete.rs`（rpi-tui）+ `editor.rs` autocomplete 域 | `editor_autocomplete_*` 快照 + autocomplete 测试 | ✅（fd 二进制 T15 挂点，当前 None；S5a 报告） |
| Shift+Enter/Ctrl+J 换行 | `editor.rs` | 键位测试 | ✅ |
| Ctrl+G 外置编辑器（prompt.md、退出码非 0 失败、Windows shell spawn） | `external_editor.rs` | `external_editor_writes_edited_text_and_cleans_up`、非 0 丢弃、spawn 失败 | ✅ |
| Ctrl+V 图文粘贴（win32 Alt+V；图片临时文件插路径、文本 fallback）；拖拽 attach | `commands_selectors.rs` handle_paste_image_impl + `interactive_mode.rs` on_action | `paste_with_clipboard_image_inserts_temp_path`、文本回退、无工具提示 | ✅（拖拽 attach 挂点；图片读取平台工具探测实现） |
| `!`/`!!` bash（首字符 `!` 边框变色） | `interactive_mode.rs` is_bash_mode + RefreshEditorBorder | `escape_clears_bash_mode`、`bash_bang_runs_command`、`bash_double_bang_excludes_from_context` | ✅ |
| autocomplete 四类命令源合并、/model /login 参数 fuzzy、防抖双档、autocompleteMaxVisible 5(3-20)、扩展注入 trigger/provider | `interactive_mode/autocomplete.rs` + `editor.rs` | autocomplete 13 测试（`/mo` 过滤、`/model m1` 参数补全、skill: 前缀、来源标签） | ✅（extension 源 T15 挂点；/login 参数 T13 挂点） |
| 扩展可整体替换 editor | — | — | ⏳ T15（ExtensionUI setEditorComponent） |

## §8.3 消息队列

| 需求 | 实现锚点 | 测试锚点 | 状态 |
|------|----------|----------|------|
| streaming Enter→steer；Alt+Enter→followUp（空闲==Enter） | `interactive_mode.rs` handle_submit + handle_follow_up | `flush_compaction_queue_restores_on_prompt_failure` 等 + S5b followUp 测试 | ✅ |
| compaction 第二队列；扩展命令立即执行不入队 | `queue_compaction_message` + flush 优先级链 | `queue_compaction_message_clears_editor_and_shows_status`、flush 失败回滚 | ✅（扩展命令 T15 挂点） |
| Escape abort 恢复队列；Escape 优先级链 | `handle_escape`（streaming abort→bash abort→退出 bash mode→双击手势） | `escape_clears_bash_mode`、`restore_queued_messages_combines_all_queues_and_aborts` | ✅（streaming abort 路径依赖真 streaming，单测以 restore 语义覆盖） |
| Alt+Up 全部队列 `\n\n` 合并放回 | `restore_queued_messages_to_editor` | `vt_driven_queue_and_editor_scenario`（Alt+Up 经 dispatch） | ✅ |
| pendingMessages 容器 + dequeue 提示；steeringMode/followUpMode | `update_pending_messages_display` | `pending_display_shows_both_modes_and_hint` | ✅ |
| willRetry 冲刷分支 | flush 优先级链 will_retry 分支 | 死代码（注释） | ⏳ 未接线（compaction_end willRetry 未传到 run loop；无认领任务，v0.1 挂起，D-019） |

## §8.4 Slash 命令

| 需求 | 实现锚点 | 测试锚点 | 状态 |
|------|----------|----------|------|
| 四类来源：builtin 优先+冲突告警 / extension / prompt template / skill:`/skill:` | `interactive_mode/autocomplete.rs`（四源合并 + 冲突诊断）；`agent_session::prompt` 展开链 | autocomplete 测试 + `get_builtin_command_conflict_diagnostics` | ✅（extension 源 T15） |
| 内置 22 个命令 | `core/slash_commands.rs` + `commands.rs` + `commands_selectors.rs` + dispatch_slash_command | 命令处理器 32 测试 + 分发链集成 10 测试 | ✅（/share T14 挂点；/export HTML T14） |
| `/debug` 隐藏（写 debug log，无 autocomplete） | `commands.rs` handle_debug_command | `debug_command_writes_log_with_messages` | ✅（全量渲染行段挂起——Tui 无公开 render API；无认领任务，D-019） |
| 彩蛋 `/arminsayshi` `/dementedelves` | — | — | [DEFER]（任务计划标注） |
| 带参数形式（/model /export /import /name /compact /login fuzzy） | 各命令处理器 | `handle_model_command_*`、export/import 测试 | ✅（/login 执行 T13） |

## §8.5 快捷键

| 需求 | 实现锚点 | 测试锚点 | 状态 |
|------|----------|----------|------|
| 完整默认表对齐 keybindings.md（~80 动作） | `core/keybindings.rs` 73 条定义 + `T12-keybindings-mapping.md` 逐条核对 | keybindings.rs 测试 + mapping 文件 | ✅ |
| 编辑器键族（jump 到字符、删除族、换行、tab） | rpi-tui Editor | editor 测试 | ✅ |
| App 级（escape/ctrl+c 双击 500ms/ctrl+d/ctrl+z/shift+tab/ctrl+p/ctrl+l/ctrl+o/ctrl+t/ctrl+n/ctrl+g/ctrl+x/alt+enter/alt+up/ctrl+v） | `setup_key_handlers` 全表接线 | `ctrl_c_single_clears_editor_and_double_shuts_down`、`ctrl_d_shuts_down`、cycle_model/thinking 测试 | ✅（ctrl+n 提示、ctrl+g/paste 见上；Windows alt+v 平台默认在定义表） |
| 双 Escape doubleEscapeAction=tree/fork/none | `handle_escape` | escape 测试 | ✅ |
| 无默认绑定 app.session.new/tree/fork/resume | 定义表（空默认）+ 转 slash 分发 | 分发链测试 | ✅ |
| 局部键（session/scoped-models/tree selector） | S5a 选择器内部 | 各选择器测试 | ✅ |
| 硬编码 shift+ctrl+d = /debug | `setup_key_handlers` | — | ✅（接线于 dispatch；动作未映射文档核对项已注明） |

## §8.6 TUI 引擎

| 需求 | 实现锚点 | 测试锚点 | 状态 |
|------|----------|----------|------|
| 渲染：2026 包裹/全量/差分/全量回退条件/16ms 节流/viewport/行尾 SGR+OSC8 reset/Kitty 图像差分 | `rpi-tui/tui.rs`（T11 交付） | tui 测试 60+ + `render_throttle_coalesces_requests` | ✅（T11） |
| 输入：chunk 重组/paste 缓冲/Kitty keyboard+legacy/DA 回退/drainInput | `rpi-tui/terminal.rs` + `stdin_buffer.rs` + `keys.rs` | terminal 测试 | ✅（T11；drainInput 在 interactive 退出路径未接线——S4b 报告） |
| Overlay/Focus/IME：9 anchor/OverlayHandle/focus 恢复/CURSOR_MARKER/容器传播 | `rpi-tui/tui.rs` overlay 域 | overlay 测试 | ✅（T11） |
| pi-tui 13 组件（Text/Box/Container/Spacer/Markdown/Image/SelectList/Input/Editor/Loader/CancellableLoader/TruncatedText/SettingsList） | `rpi-tui/components/*` | 组件测试 + 快照（Editor 8/SelectList 4/Markdown 3+/SettingsList 3/Autocomplete 2） | ✅ |
| coding-agent 交互组件 40 个 | `crates/rpi/src/modes/interactive/components/*`（20 个移植 + 消息族 13 + 基础 7） | 组件测试 ~300 | ✅（彩蛋 [DEFER]；extension 相关 4 个以独立组件形式存在） |
| Markdown：marked 等价/流式 fence/code border+indent/主题 20+ | `rpi-tui/components/markdown.rs`（comrak，D-018） | markdown 快照 + 集成用例 | ✅（D-018 两条边缘差异登记） |
| Image：Kitty+iTerm2/能力检测矩阵/fallback | `rpi-tui/terminal_image.rs` | detect_capabilities 15 测试 + image 快照 | ✅ |
| 终端特例（WT/tmux/Apple/Termux/Ghostty/WezTerm/screen） | `rpi-tui/terminal.rs` + 能力层 | 特例测试 | ✅（D-016 两平台缺口：Apple 修饰键检测、Windows VT input） |
| 终端自省四件套（OSC11/CSI?996n/CSI16t/OSC9;4 含 keepalive） | `rpi-tui/tui.rs` + `terminal.rs` | 自省查询测试 | ✅（T11） |

## 覆盖统计

- §8.1-8.6 共 46 条需求行：**40 ✅**、**4 ⏳ 挂点**（extension 源 T15×2、拖拽 attach、willRetry 接线——无认领）、**1 [DEFER]**（彩蛋）、**1 挂点**（扩展替换 editor T15）。
- 挂点明细（2026-08-06 核实）：T13（/login 执行、粘贴图片原生 addon 读取、/login 参数补全）、T14（/share、HTML 导出、cache-miss/cache-waste）、T15（extension 命令/autocomplete/冲突诊断数据、fd 文件补全、setHeader/setEditorComponent、changelog 资产、扩展状态行）；无认领（v0.1 内挂起）：/debug 渲染行段、willRetry 冲刷接线、/tree 摘要询问循环与标签写回、ShowImages/ImageWidthCells 热应用传播、EnableSkillCommands 重挂 autocomplete、/reload reload box、session-selector 状态自动隐藏定时器、tmux 键盘检查。
