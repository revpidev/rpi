# T12：pir-tui 组件与 Interactive 模式

- **状态**：未开始
- **里程碑**：M5（TUI 为硬性交付，ADR-0002 §3）
- **依赖**：T10、T11
- **上游对照**：`packages/tui/src/components/*`、`packages/coding-agent/src/modes/interactive/*`、`docs/keybindings.md`
- **需求章节**：§2.1、§8（全章）、§3.1（交互相关标志）
- **预估**：5–7 人月（M5 共 8–11，与 T11 合计）

---

## 目标

交付完整 Interactive 模式：组件库 + 会话绑定 + slash 命令 + 消息队列 UX，
达到 M5「Interactive TUI 可用」的必达标准。

## 范围

### In

- 组件（按设计文档 §5.5 优先级）：`SelectList` / `Input` / `Editor`（多行、undo、kill-ring、bracketed paste 大粘贴 marker、`@` 文件模糊、Tab 路径补全、Shift+Enter、Ctrl+G 外置编辑器、Ctrl+V 图文粘贴、`!`/`!!` bash）/ `Autocomplete` / `Markdown`（流式 fence 稳定）/ `Loader` / `Box` / `SettingsList` / `Image`（Kitty/iTerm2）
- 布局：startup header → messages → editor → footer（cwd、session 名、tokens/cache/cost/context、model）
- 组件树绑定 session 事件；选择器与 slash 路由
- 内置 slash 命令（需求 §8.4 全集）：`/login` `/logout` `/model` `/scoped-models` `/settings` `/resume` `/new` `/name` `/session` `/tree` `/trust` `/fork` `/clone` `/compact` `/copy` `/export` `/import` `/reload` `/hotkeys` `/changelog` `/quit`（`/llama` 随 T14、`/share` 随 T14）
- 消息队列 UX：Enter steering / Alt+Enter follow-up / Escape abort 恢复队列 / Alt+Up 取回
- 快捷键（需求 §8.5）：Ctrl+C/双 Ctrl+C、Escape/双 Escape、Ctrl+L、Ctrl+P、Shift+Tab、Ctrl+O、Ctrl+T、Ctrl+X 等
- Project trust 交互提示（两阶段加载的交互面）
- 主题应用与热重载；`/debug`（环形缓冲最近渲染行 + 最近 LLM context 快照）
- 未知 custom entry 通用 JSON 折叠块渲染（需求 §6.5）

### Out

- 扩展 UI（dialog/widget/overlay/custom editor，T15）
- Packages 相关 slash 交互（T14）

## 开发要点

- Editor 是最大单组件（上游 2.3k LOC），按上游内部结构分模块移植，保持文件对应
- slash 命令与快捷键逐条对照上游文档建核对清单
- 渲染快照黄金文件覆盖 Editor / SelectList / Markdown / SettingsList（需求 §11.1）
- 性能：大 session 滚动可用（需求 §11.2），构造大 fixture 验证

## 进度跟踪

- [ ] 设计细化
- [ ] 实现
- [ ] 自测
- [ ] 门禁验收
- [ ] 文档回写

## 自测清单

- [ ] 组件快照黄金文件全过（Editor / SelectList / Markdown / SettingsList / Autocomplete）
- [ ] 消息队列四操作行为与上游一致（VT 驱动测试）
- [ ] slash 命令全集逐条可用（清单核对 + 关键命令 VT 测试）
- [ ] 快捷键全集与默认表一致，JSON 覆盖生效
- [ ] trust 两阶段加载时序：信任前仅 context+全局/CLI 扩展，信任后加载项目资源
- [ ] 主题热重载生效；`/debug` 输出包含渲染行与 LLM context 快照
- [ ] 大 session（构造 1k+ 消息 fixture）滚动与渲染流畅（节流不丢帧）

## 门禁验收

通用门禁 G1–G7 全过。

任务特有标准：

- [ ] 需求 §8 全章逐条核对有锚点（验收记录列映射表）
- [ ] 真机 smoke 矩阵：本机 + tmux 至少两种环境人工验证（启动、提问、streaming、abort、快捷键、退出恢复）
- [ ] M5 必达声明：Interactive 模式完成一次端到端真实对话（faux 或 live）

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| — | （暂无） | — |

## 验收记录

（待填写，模板见 `gates.md` §3）
