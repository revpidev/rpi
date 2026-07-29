# T12：pir-tui 组件与 Interactive 模式

- **状态**：未开始
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

- [ ] 设计细化
- [ ] 实现
- [ ] 自测
- [ ] 门禁验收
- [ ] 文档回写

## 自测清单

- [ ] 组件快照黄金文件全过（Editor / SelectList / Markdown / SettingsList / Autocomplete）
- [ ] 消息队列行为（含 compaction 第二队列、Alt+Up 合并全部、Escape 优先级链）与上游一致（VT 驱动测试）
- [ ] slash 命令 22+1 逐条可用（清单核对 + 关键命令 VT 测试）；四类来源冲突处理（内置优先告警）
- [ ] 快捷键全表与 `docs/keybindings.md` 逐条核对；JSON 覆盖生效；局部键与双击手势
- [ ] trust 两阶段加载时序 + 5 选项弹窗各分支
- [ ] footer/header 显示口径（token 五项、着色阈值、截断、Ctrl+O 联动）
- [ ] `/tree` 选中行为三分支
- [ ] 主题热重载生效；`/debug` 输出包含渲染行与 LLM context 快照
- [ ] 图像能力检测矩阵各终端分支
- [ ] 大 session（构造 1k+ 消息 fixture）滚动与渲染流畅（节流不丢帧）

## 门禁验收

通用门禁 G1–G7 全过。

任务特有标准：

- [ ] 需求 §8 全章逐条核对有锚点（验收记录列映射表）
- [ ] `keybindings.md` 默认绑定表逐条对拍映射表（G3）
- [ ] 真机 smoke 矩阵：本机 + tmux 至少两种环境人工验证（启动、提问、streaming、abort、快捷键、退出恢复）
- [ ] M5 必达声明：Interactive 模式完成一次端到端真实对话（faux 或 live）

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| — | （暂无） | — |

## 验收记录

（待填写，模板见 `gates.md` §3）
