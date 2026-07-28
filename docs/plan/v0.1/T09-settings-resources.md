# T09：Settings 与资源加载

- **状态**：未开始
- **里程碑**：M3
- **依赖**：T01
- **上游对照**：`docs/settings.md`、`docs/skills.md`、`docs/prompt-templates.md`、`docs/themes.md`、`docs/keybindings.md`；`packages/coding-agent/src/core/` 资源加载相关模块
- **需求章节**：§7.1–§7.5、§7.7
- **预估**：0.5–1 人月（M3 共 2–3，与 T07/T08 合计）

---

## 目标

实现 SettingsManager 与 ResourceLoader 统一发现管线，使声明式资源
（context files / skills / prompt templates / themes / keybindings）与 Pi 格式互通。

## 范围

### In

- `SettingsManager`：全局 `~/.pir/agent/settings.json` + 项目覆盖；完整对齐 `docs/settings.md`（model / UI / compaction / retry / transport / packages / telemetry 等字段）
- `ResourceLoader` 统一发现：全局 → 项目 → settings 路径 → CLI flags → packages，输出 `LoadedResources { extensions, skills, prompts, themes, context_files, diagnostics }`（设计文档 §6.7）
- Context files：`AGENTS.md` / `CLAUDE.md`（全局 + 祖先链 + cwd）；`SYSTEM.md` / `APPEND_SYSTEM.md` 覆盖/追加
- Skills：发现路径（含 `~/.agents/skills`、祖先 `.agents/skills`）、frontmatter 解析、渐进披露（system prompt XML 摘要注入，全文 on-demand）、`disable-model-invocation` 等语义
- Prompt templates：`*.md` → 命令名；`$1` / `$@` / `${1:-default}` 展开规则
- Themes：内置 dark/light + 自定义 JSON（51 color tokens）解析与校验
- Keybindings：`keybindings.json` 解析，token 名与 Pi 一致

### Out

- 主题热重载与 TUI 应用（T12）
- Packages 安装来源加载（T14；本任务管线预留 packages 输入口）
- Project trust 两阶段加载（T14；本任务提供「信任前/后」分组能力）

## 开发要点

- 发现顺序与覆盖语义逐项对照上游文档，冲突合并规则写测试锁死
- 文件格式兼容用 fixtures 验证（skills / prompts / themes / keybindings 各取上游样例）
- 新增配置路径一律走统一路径模块，不散落拼接（编码规范 §10.1）

## 进度跟踪

- [ ] 设计细化
- [ ] 实现
- [ ] 自测
- [ ] 门禁验收
- [ ] 文档回写

## 自测清单

- [ ] settings 合并语义：全局 + 项目覆盖、缺省值、非法 JSON 诊断
- [ ] 资源发现顺序与去重规则测试（构造多级目录 fixture）
- [ ] skills：XML 摘要注入内容、frontmatter 各字段语义与上游一致
- [ ] prompt template 展开：`$1` / `$@` / 默认值各形态与上游一致
- [ ] themes：51 token 解析、缺失 token 回退、非法值诊断
- [ ] keybindings：token 名与上游文档逐条核对

## 门禁验收

通用门禁 G1–G7 全过（G5 重点：settings/themes/keybindings JSON 形状兼容）。

任务特有标准：

- [ ] 需求 §7.1–§7.5、§7.7 各条目有测试锚点（验收记录列映射表）
- [ ] 上游资源样例 fixtures 加载结果对拍一致

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| — | （暂无） | — |

## 验收记录

（待填写，模板见 `gates.md` §3）
