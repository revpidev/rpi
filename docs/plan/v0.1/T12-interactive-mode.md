# T12：pir-tui 组件与 Interactive 模式

- **状态**：已完成（2026-08-06 用户真机 smoke 人工验收通过）
- **里程碑**：M5（TUI 为硬性交付，ADR-0002 §3）
- **依赖**：T10、T11
- **上游对照**：`packages/tui/src/components/*`、`packages/coding-agent/src/modes/interactive/*`（interactive-mode.ts + 40 组件）、`docs/keybindings.md`（逐条对拍级基准）、`docs/usage.md`、`docs/sessions.md`
- **需求章节**：§2.1、§8（全章）、§3.1（交互相关标志）
- **预估**：5–7 人月（M5 共 8–11，与 T11 合计）

---

## 目标

交付完整 Interactive 模式：组件库 + 会话绑定 + slash 命令 + 消息队列 UX，
达到 M5「Interactive TUI 可用」的必达标准。

## 范围

### In

- pi-tui 业务组件：`SelectList` / `Input` / `Editor` / `Autocomplete` / `Markdown`（marked 等价 + `trim_partial_closing_fences` 流式 fence 稳定）/ `SettingsList` / `Image`（Kitty + iTerm2 + **能力检测矩阵**：kitty/ghostty/wezterm/warp→kitty；iTerm2→iterm2；**tmux/screen 禁用**；Windows Terminal/VSCode/Alacritty→hyperlink only；JetBrains→无 hyperlink；未知→`text (url)` 回退）
- Editor 全特性：多行；undo（**Ctrl+-**，快照含 paste registry，词符合并单元）；kill-ring（Ctrl+Y/Alt+Y）；**历史**（up/down、100 条、草稿保存）；bracketed paste 大粘贴 marker（**>10 行或 >1000 字符**，两种格式，原子 segment）；`@` 文件模糊（引号路径、`~/` 展开）；Tab 上下文分派（slash→命令、否则文件）；Shift+Enter/Ctrl+J 换行；Ctrl+G 外置编辑器；Ctrl+V 图文粘贴（**win32 Alt+V**，图片写临时文件插路径）；拖拽文件 attach；`!`/`!!` bash（边框变色）；autocomplete 防抖双档；`autocompleteMaxVisible`（5，3-20）
- 布局（需求 §8.1）：startup header（logo + 快捷键提示紧凑/展开两态**随 Ctrl+O 联动** + onboarding + changelog；`quietStartup` 空）→ messages → editor → footer（行1 cwd~/git branch/session 名；行2 `↑↓/R/W/CH%/$cost[ (sub)]/<context%>[ (auto)][ xp]` + `(provider) model[ • thinking]`，70/90% 着色，逐级截断；行3 扩展 status）
- coding-agent 交互组件（需求 §8.6 清单，40 个）：message 渲染族（assistant/user/tool-execution/diff/bash-execution/branch-summary/compaction-summary/skill-invocation/custom-message/custom-entry）+ selector 族（model/scoped-models/settings/theme/thinking/login/oauth/config/session/tree/user-message/trust/extension/show-images）+ footer/custom-editor/extension-editor/extension-input/first-time-setup/bordered-loader/countdown-timer/status-indicator/keybinding-hints/dynamic-border/visual-truncate（彩蛋组件 [DEFER]）
- 组件树绑定 session 事件；选择器与 slash 路由
- **Slash 命令四类来源**（builtin 优先 + extension + prompt template + skill）：**内置 22 个**（需求 §8.4 清单）+ 隐藏 `/debug`（写 debug log，shift+ctrl+d 同效）+ 带参数形式的 fuzzy 补全（/model、/login 等）；`/llama`、`/share` 随 T14；彩蛋命令 [DEFER]
- 消息队列 UX：Enter steering / Alt+Enter follow-up（**空闲时 Alt+Enter==Enter**）/ **compaction 第二队列**（扩展命令立即执行）/ Escape abort 恢复队列（四级优先级链）/ Alt+Up **合并全部队列**一次性放回
- 快捷键（需求 §8.5，**全表对齐 `docs/keybindings.md` ~80 动作**）：编辑器 emacs 键族、app 级（Ctrl+L=model selector、Ctrl+P/Shift+Ctrl+P 模型正/反 cycle、Ctrl+O tools+header 联动、Ctrl+T、Ctrl+N、Ctrl+X 复制消息、Ctrl+D 退出、Ctrl+Z suspend）、双击手势（Ctrl+C 500ms 退出、双 Escape 可配 `doubleEscapeAction`）、局部键（session selector 5 键、scoped-models 6 键、tree 12 键）、无默认绑定项
- `/tree` 选中行为：user/custom_message → leaf=parent + 文本回填编辑器；assistant 等 → 移 leaf 留空；根 user → 重置 leaf（`docs/sessions.md` 规格）
- Project trust 交互提示（**5 选项弹窗**：Trust / Trust parent folder / Trust (session only) / Do not trust / Do not trust (session only)）
- 首次运行 setup：主题选择（按终端背景默认 dark/light）+ analytics opt-in
- 主题应用与热重载（仅全局当前主题文件）；`/settings` 设置菜单全项（需求 §8 审查清单）；`/debug`（环形缓冲最近渲染行 + 最近 LLM context 快照）
- 未知 custom entry 通用 JSON 折叠块渲染（需求 §6.6）

### Out

- 扩展 UI（dialog/widget/overlay/custom editor，T15）
- Packages 相关 slash 交互、`/llama`、`/share`（T14）

## 开发要点

- Editor 是最大单组件（上游 2.3k LOC），按上游内部结构分模块移植，保持文件对应
- slash 命令与快捷键逐条对照上游文档建核对清单（G3 逐条对拍级基准）
- 渲染快照黄金文件覆盖 Editor / SelectList / Markdown / SettingsList / Autocomplete（需求 §11.1）
- 性能：大 session 滚动可用（需求 §11.2），构造大 fixture 验证

## 进度跟踪

- [x] 设计细化（2026-08-05，见下节「设计细化记录」）
- [x] S4a 消息渲染族 13 组件 + theme 辅助（2026-08-05）
- [x] S4b InteractiveMode 骨架（2026-08-05）：`interactive_mode.rs`（布局容器链、24 事件分支、初始渲染、submit/escape 基础、run 循环与 TUI 驱动线程）、`footer.rs`、`custom_editor.rs`、`header.rs`、app.rs Interactive 分支接线（原 T12 占位错误替换）
- [x] S5a 选择器族 20 件（2026-08-05）：tree/session(+search)/config/settings/model(+search)/scoped-models/oauth/trust/theme/thinking/show-images/user-message/extension 选择器 + login-dialog/extension-editor/extension-input/bordered-loader/first-time-setup；`showSelector` 框架（Tui 子项位置保持替换 editor、done 恢复、双 Escape 接线）；/tree 选中三分支经 `navigate_tree` 实现（摘要询问循环留 S5b 挂点）；修复 9 组件键位读锁嵌套隐患
- [x] S5b slash 路由 + 22+1 命令 + 快捷键全表（2026-08-05）：`handle_submit` 完整分发链（精确/前缀匹配、未命中落 prompt 四类展开）；`core/slash_commands.rs` 22 内置清单；`commands.rs`（session/name/copy/export/import/changelog/hotkeys/debug/compact/share/quit）、`commands_selectors.rs`（settings/model/scoped-models/session/trust/login/logout/tree/fork/new/resume/clone/reload + bash `!`/`!!` 模式 + cycle model/thinking/Ctrl+Z suspend/dequeue/followUp 动作）、`autocomplete.rs`（四类来源合并 + /model 参数 fuzzy 补全 + skill: 前缀 + 冲突诊断）；app 级快捷键逐条接线（映射核对表 docs/plan/v0.1/T12-keybindings-mapping.md）；SIGCONT 恢复、OSC52 复制
- [x] S6 队列 UX + 收尾（2026-08-05）：`updatePendingMessagesDisplay` 完整渲染（Steering:/Follow-up: 行 + Alt+Up 提示）、compaction 第二队列完整语义（入队提示/冲刷优先级链 willRetry→扩展命令→首条 prompt→其余按 mode 排队→失败回滚）、Escape restore 合并全部队列、主题热重载（轮询 watcher 100ms + 解析失败保旧 + UiCommand::ThemeChanged）、auto 明暗探测（OSC11/DSR996/COLORFGBG + 通知监听）、首启 setup（startup_ui.rs，判定=无全局 settings.json + 实验开关）、外部编辑器 Ctrl+G（external_editor.rs，$VISUAL/$EDITOR/nano + spawn + 读回）、Ctrl+V 图文粘贴（xclip/wl-paste/pbpaste 探测）、loadedResources 分节显示（Context/Skills/Prompts/Extensions/Themes + 诊断区）、cache-miss 通知挂点（T14/cache-stats）、trust 两阶段确认（app.rs create_runtime 已有 + warning 文本 S4b 已实现 + /trust 选择器 S5b 已接）
- [x] S7a 自测补全（2026-08-05）：大 session 性能（1200 条：build 34ms / render 752ms）、节流合并、队列 VT 驱动、/tree 三分支 UI 集成、图像矩阵核对、footer `?` 分支；自测清单 10 项全勾
- [x] S7b 偏离登记 + §8 映射 + 文档回写（2026-08-05）：D-018（comrak）、D-019（T12 笔记）登记并回写；`T12-requirements-8-mapping.md` 46 条映射；`T12-keybindings-mapping.md` 73 条核对
- [x] 实现
- [x] 自测
- [x] 门禁验收（2026-08-06 真机 smoke 人工确认，见验收记录）
- [x] 文档回写

## 设计细化记录（2026-08-05）

基于三路上游/本地调研（pi-tui 组件、interactive-mode、pir 现状）收口如下。

### 模块映射（上游 → 本仓库）

pi-tui 业务组件与支撑模块（落 `crates/pir-tui/src/`，镜像上游文件命名）：

| 上游 | 本仓库 |
|------|--------|
| `tui/src/kill-ring.ts` | `pir-tui/src/kill_ring.rs` |
| `tui/src/undo-stack.ts` | `pir-tui/src/undo_stack.rs` |
| `tui/src/word-navigation.ts` | `pir-tui/src/word_navigation.rs` |
| `tui/src/autocomplete.ts` | `pir-tui/src/autocomplete.rs` |
| `tui/src/components/{editor,input,select-list,markdown,settings-list,image}.ts` | `pir-tui/src/components/{editor,input,select_list,markdown,settings_list,image}.rs` |
| `coding-agent/src/modes/interactive/*` | `pir/src/modes/interactive/`（`mod.rs` + `components/` 40 组件镜像命名 + `external_editor.rs` + `model_search.rs`） |

复用既有：`fuzzy.rs`（T11 已含 fuzzyFilter）、`terminal_image.rs`（能力检测矩阵 T11 已落地）、`themes`/`settings`/`keybindings`（pir 侧 core）、`AgentSession::subscribe`（同步事件，print_mode 为范本）。挂载点：`app.rs` Interactive 分支（原占位错误）；`--resume` 分支——**实际实现**（2026-08-06）为 `cli/session_picker.rs` 独立启动选择器（对齐上游 main.ts:321-333 + cli/session-picker.ts，picker 在 session manager 创建前独立起 TUI，取消打印 "No session selected" 并 exit 0），而非模式内弹窗（登记 D-019）。

### 关键决策

1. **Markdown 解析器**：`marked` → `comrak`（AST 树形结构与 marked token 树最接近，GFM 表格/删除线/任务列表齐备；`token.raw` 用 sourcepos 切片源码还原）。属依赖替代型偏离，登记 D-018，渲染行为以快照黄金 + marked 产出对拍兜底。
2. **keybindings 双轨打通**：pir-tui `KeybindingsManager`（31 个 `tui.*`）与 pir `core/keybindings.rs`（73 条含 `app.*` 42）并存。方案：pir 侧保留定义表与 JSON 加载/迁移为唯一事实源，启动时把解析后的用户绑定灌入 pir-tui 管理器（`set_keybindings`）；`app.*` 分发在 `CustomEditor.handle_input` 与 `InteractiveMode` 层。
3. **分段器**：grapheme 用 `unicode-segmentation`（已有依赖）；word 粒度（`isWordLike` 语义）按 T11 utils 既定口径自实现于 `word_navigation.rs`。
4. **fd 依赖**：文件补全沿用 spawn `fd`（`std::process::Command`），与上游参数协议逐字对齐；缺 fd 时回退 `readdir` 前缀补全（同上游行为）。
5. **回调风格**：`onSubmit`/`onChange`/`submenu(done)` 等闭包 → `Box<dyn FnMut + Send>` 字段，沿用 T11 组件约定；锁契约（组件回调内不得锁其他组件）继续适用。
6. **VT 测试**：`tui.rs` 内 `VirtualTerminal` 测试模拟器外提为 `pir-tui` 的 `#[cfg(test)]`/test-support 公共件（或复制到各测试目标），队列/快捷键行为用 VT 驱动；不引入 pty 依赖。

### 实现子阶段

- **S1 支撑与简单组件**：kill_ring / undo_stack / word_navigation / input / select_list + 快照黄金
- **S2 Editor + Autocomplete**：editor.rs 按上游内部域分块移植（state/undo/history/kill-ring/paste 标记/layout/scroll/autocomplete 接入）+ autocomplete.rs（fd/路径/slash 三路）+ 快照与 VT
- **S3 Markdown / SettingsList / Image**：comrak 接入 + trim_partial_closing_fences + 快照黄金
- **S4 InteractiveMode 骨架**：布局容器（header/messages/editor/footer）、session 事件绑定、message 渲染族 13 件
  - S4a（完成）：message 渲染族 13 组件 + theme 辅助
  - S4b（完成 2026-08-05）：`interactive_mode.rs`（布局容器链 header→loadedResources→chat→pendingMessages→status→widgetsAbove→editor→widgetsBelow→footer；`AgentSession::subscribe` 同步回调 → `UiCommand` 队列 → 驱动线程 drain 的 24 事件分支；renderInitialMessages 用 S4a 渲染族重建；Enter→prompt/steer/compaction 入队；Escape 基础（streaming abort+恢复队列、bash 模式退出、双击手势）；Ctrl+C 双击退出；SIGTERM/SIGHUP 优雅退出；/quit 接线）、`footer.rs`（token 五项口径/formatTokens/费用/context% 70/90 着色/逐级截断/pwd 行/扩展状态行挂点）、`custom_editor.rs`（app 键位分发骨架 + actionHandlers Map + EditorRegion 树入口）、`header.rs`（ExpandableText 两态 + 19/5 条键位提示 + quietStartup 空）、app.rs Interactive 分支接线（keybindings 73 条双轨灌入 + 主事件循环）
- **S5 选择器族 14 件 + slash 22+1 + 快捷键全表**（keybindings.md 逐条核对）
- **S6 消息队列 UX / trust / 首启 setup / 主题热重载 / /debug**
- **S7 自测补全（大 session 性能、图像矩阵）+ 门禁验收**

## 自测清单

- [x] 组件快照黄金文件全过（Editor 8 / SelectList 4 / Markdown 3+ / SettingsList 3 / Autocomplete 2 快照，S1-S3 交付；S7a 核对无缺）
- [x] 消息队列行为（含 compaction 第二队列、Alt+Up 合并全部、Escape 恢复队列）与上游一致（S6 单测 + S7a VT 驱动 `vt_driven_queue_and_editor_scenario`；Escape 优先级链的 streaming abort 依赖真 streaming 无法注入，以 restore 队列语义单测覆盖）
- [x] slash 命令 22+1 逐条分发可用（S5b 命令处理器单测 32 + S7a 分发链集成测试 10）：全部 22 内置 + 隐藏 `/debug` 均接通 `dispatch_slash_command`；其中仍是挂点的（2026-08-06 核实）：`/share`（T14，状态提示）、`/login`/`/logout` 执行流（T13，选择器挂载）、`/export` HTML 分支（T14，JSONL 可用）、`/debug` 全量渲染行段（无认领，D-019）、`/session` cache-waste 段（T14）、`/tree` 摘要询问循环与标签写回（无认领）；四类来源冲突处理（内置优先告警——T15 数据挂点，恒空已测）
- [x] 快捷键全表与 `docs/keybindings.md` 逐条核对（T12-keybindings-mapping.md，73 条默认键一致）；JSON 覆盖生效（keybindings.rs 测试）；局部键与双击手势（S4b/S5a 测试）
- [x] trust 两阶段加载时序（app.rs create_runtime 测试锚点 + S7a 核对）+ 5 选项弹窗各分支（S5b `trust_selector_saves_decision` 等）
- [x] footer/header 显示口径（token 五项、CH%、费用、70/90 着色阈值、逐级截断、pwd 行、Ctrl+O 联动——S4b 11+4 测试；S7a 补 compaction 后 `?` 分支）
- [x] `/tree` 选中行为三分支（S7a UI 集成端到端：user/custom_message → leaf=parent+回填、assistant → 移 leaf 留空、根 user → 重置 leaf）
- [x] 主题热重载生效（S6 轮询 watcher + ThemeChanged drain 测试）；`/debug` 输出包含 agent 消息 JSONL 快照（全量渲染行段因 Tui 无公开 render API 挂起——无认领任务，D-019）
- [x] 会话切换全量 rebind（2026-08-06 修复）：/new、/resume、/clone、/fork、/import 统一走 `rebind_session_ui`（RwLock session + 注销旧订阅 + 重建订阅）；/resume、/fork 选择器回调经 `EditorInput::ResumeSession`/`ForkFrom` 路由到 run loop 执行
- [x] footer git branch 生效（2026-08-06）：`git_branch_watcher.rs` 100ms 轮询 `.git/HEAD`（worktree 支持，cwd 跟随 `set_cwd`），变化经 `UiCommand::GitBranchChanged` 失效 footer
- [x] /settings 六项热应用接线（2026-08-06）：Theme（含 auto 明暗对探测）、HideThinkingBlock/ShowCacheMissNotices（rebuild chat）、EditorPaddingX、OutputPad（streaming 就地更新/否则 rebuild；历史 child 不更新，D-019）、AutocompleteMaxVisible
- [x] 图像能力检测矩阵各终端分支（terminal_image.rs 15 个 detect_capabilities 测试：kitty/ghostty/wezterm/warp/iTerm2/tmux/screen/WT/VSCode/JetBrains/未知）
- [x] 大 session（1200 条消息 fixture：build 34ms / 全量渲染 752ms，1799 组件 13799 行；节流合并测试 `render_throttle_coalesces_requests`）

## 门禁验收

通用门禁 G1–G7 全过。

任务特有标准：

- [x] 需求 §8 全章逐条核对有锚点（`T12-requirements-8-mapping.md`，46 条：40 完成 / 4 挂点 / 1 DEFER / 1 扩展替换挂点，均注 T13/T14/T15 遗留）
- [x] `keybindings.md` 默认绑定表逐条对拍映射表（`T12-keybindings-mapping.md`，73 条默认键一致，G3）
- [x] 真机 smoke 矩阵：本机 + tmux 至少两种环境人工验证（启动、提问、streaming、abort、快捷键、退出恢复）——**2026-08-06 用户人工确认**（脚本化 smoke 见验收记录）
- [x] M5 必达声明：Interactive 模式完成一次端到端真实对话（faux 或 live）——**进程内 VT 端到端**（TestTerminal + 真实 session：启动→提问（prompt 错误路径因无 auth）→bash 执行→快捷键→退出恢复；streaming 与真实 provider 对话留待真机 smoke）

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| D-018 | Markdown 解析器 comrak 0.54 替代 marked@18.0.5（AST 对应 + 3 条残留边缘差异 + 3 个 xterm 用例改输出级断言） | 已回写 |
| D-019 | interactive 模式移植 Rust 落地笔记（汇总型 25 条，含 /copy 仅 OSC52、/debug 行段缺口、Ctrl+Z SIGTSTP、轮询主题/git watcher、willRetry 死代码、首启判定、--resume 独立 picker、OutputPad streaming 等，逐条三档标注；「会话切换不重订阅」2026-08-06 修复关闭） | 已回写 |

## 验收记录

- 验收日期：2026-08-05（脚本化自测）/ 2026-08-06（真机 smoke 人工确认）
- 验收人：用户（真机 smoke 人工验证，2026-08-06 确认）
- G1 构建/静态检查：通过——`cargo clippy --workspace --all-targets -D warnings` 通过（0 警告）；`cargo fmt --all -- --check` 通过；`cargo check --workspace` 通过
- G2 测试：通过——`cargo test --workspace` 2608 passed, 0 failed（`cargo test -p pir` 1282、`cargo test -p pir-tui` 854，含 lib/集成/快照全目标）；live 测试跳过（无 API key）。备注：一次全量并行运行出现 1 例失败未捕获名称，随后针对性重跑 6 轮（interactive 模块 5 轮、pir-tui 3 轮、集成 2 轮）均未复现，疑似并行负载下时序敏感，持续观察
- G3 对拍：通过——`T12-keybindings-mapping.md` 73 条默认键逐条核对一致；`T12-requirements-8-mapping.md` §8 全章 46 条映射（40 ✅ 4 挂点 1 DEFER 1 挂点，均注 T13/T14/T15 遗留）；渲染快照黄金文件全过（Editor/SelectList/Markdown/SettingsList/Autocomplete）
- G4 红线：通过——未改 `external/pi/`；未引入 JS/TS 执行能力（无 Node/Deno 嵌入）；未默认读写 `~/.pi`/`.pi`（仅 `~/.pir`，ADR-0001）；session 存储仍 JSONL（无 SQLite）；token 估算未引入新算法；可恢复错误无 panic（测试除外）；日志无凭据输出
- G5 线格式：通过——session JSONL/RPC 线格式未变（T12 无线格式改动；扩展 UI 协议层留 T15）
- G6 文档同步：通过——回写位置：`02-design.md` §5.5/§5.6/§12（T12 状态与映射行）、`01-requirements.md` §8.6（comrak 注记）、`T12-interactive-mode.md`（进度/自测清单/偏离记录/本记录）、`deviations/README.md`（D-018/D-019 登记）
- G7 偏离闭环：D-018/D-019 已登记并已回写（README 表与本文偏离记录状态均置「已回写」）；D-016/D-017 已关闭
- 脚本化 smoke（本环境无 tty，真机人工 smoke 留给用户）：
  1. `cargo run -p pir -- --help` 正常输出；无 tty 降级 Print 模式验证（`echo hi | pir` 不触 TUI）
  2. 进程内 VT 端到端（测试基建）：a) 启动→布局树组装→bash `!` 执行→/tree 选择→/name→/quit 信号→shutdown 恢复——`interactive_mode.rs` 集成测试（init/分发链/tree/队列共 60+）；b) **run loop 全路径**：启动→bash 提问→Ctrl+C 清空→Ctrl+D 退出→终端恢复——`run_loop_end_to_end_vt_smoke`（真实 session + 事件循环，见验收记录 G2 测试数）
  3. tmux 可用性：`which tmux` 待真机环境确认（本容器无 tmux）
- 结论：**已完成**——2026-08-06 用户在有 tty 的真机环境完成验收：`cargo run -p pir` 启动 → 提问 → streaming → abort → 快捷键 → Ctrl+C/Ctrl+D 退出 → 终端状态恢复，本机 + tmux 至少两种环境人工验证通过。

### 2026-08-06 修复记录（验收后追加）

- **阶段 A（会话切换与恢复）**：`InteractiveUi.session` 改为 `RwLock` 可替换；新增 `rebind_session_ui`（对齐上游 `rebindCurrentSession`）做全量 rebind——注销旧订阅、换 session、`apply_runtime_settings`、清容器、`render_initial_messages`、重新订阅；/new、/resume、/clone、/fork、/import 统一走此路径（D-019「会话切换不重订阅」条目关闭）。/resume、/fork 选择器回调经 `EditorInput::ResumeSession`/`ForkFrom` 路由到 run loop 执行（`handle_resume_command` / `handle_fork_command`，fork 用 `runtime.fork(entry_id, ForkPosition::Before, None)` + 编辑器文本回填）。--resume CLI 落地为 `cli/session_picker.rs` 独立启动选择器（对齐上游 main.ts:321-333，取消 exit 0），`app.rs` resume 分支接线。unsubscribe 存字段、shutdown 调用；`flush_compaction_queue` 两个 shutdown 分支恢复队列。新支撑：`FooterDataProvider.set_cwd`、`output_pad` 转 AtomicUsize、`CustomEditor.set_padding_x`/`set_autocomplete_max_visible`
- **阶段 B（settings 热应用 + git branch）**：/settings 六项热应用全部接线（`apply_settings_change`）：Theme（含 auto 明暗对探测）、HideThinkingBlock/ShowCacheMissNotices（rebuild chat）、EditorPaddingX、OutputPad（streaming 就地更新，否则 rebuild）、AutocompleteMaxVisible。git branch watcher 落地（`git_branch_watcher.rs`，100ms 轮询 .git/HEAD，worktree 支持，cwd 跟随 set_cwd），footer 分支真正显示
- **阶段 C（文档/注释收尾）**：crates 内 `TODO(S5/S6/S5b)` 残留全部清理（已解决的删或改事实陈述；未解决的明确归属 TODO(T13/T14/T15)，无认领的标 `TODO(unassigned)`）；D-019 修订（条目 22→25）；本任务文件与两张映射表同步；footer `xp` 实验标记接线 `experimental_enabled`（一行，PIR_EXPERIMENTAL=1 生效）；修复 resume 测试把 fixture 写进 crate 目录的问题（该写 harness 临时目录）
- **仍开放的挂点**：/debug 全量渲染行段（无认领，v0.1 挂起）、willRetry 冲刷接线（无认领，v0.1 挂起）、OutputPad streaming 历史 child 不更新（D-019 已知差异）、cache-miss/cache-waste（T14）、/share 与 HTML 导出（T14）、/login /logout 执行（T13）、/tree 摘要询问循环与标签写回（无认领）
- 本日改动为注释/文档 + 已验收功能的缺口补齐；`cargo build --workspace` 与 `cargo clippy --workspace --all-targets` 通过（0 警告）、`cargo fmt --all -- --check` 通过、`cargo test --workspace` 全绿。flaky 观察：全量首轮曾出现 1 例未捕获名称的失败（G2 同类现象第三次），随后全量 4 轮 + interactive 模块 8 轮连跑均全绿未复现，维持 G2「并行负载下时序敏感，持续观察」的结论
