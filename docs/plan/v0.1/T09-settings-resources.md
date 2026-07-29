# T09：Settings 与资源加载

- **状态**：未开始
- **里程碑**：M3
- **依赖**：T01
- **上游对照**：`docs/settings.md`、`docs/skills.md`、`docs/prompt-templates.md`、`docs/themes.md`、`docs/keybindings.md`、`docs/environment-variables.md`、`docs/security.md`；`packages/coding-agent/src/core/{settings-manager,resource-loader,skills,prompt-templates,keybindings,system-prompt}.ts`
- **需求章节**：§3.3（环境变量）、§7.1–§7.5、§7.7
- **预估**：0.7–1 人月（M3 共 3–3.5，与 T07/T08/T16 合计）

---

## 目标

实现 SettingsManager 与 ResourceLoader 统一发现管线，使声明式资源
（context files / skills / prompt templates / themes / keybindings）与 Pi 格式互通。

## 范围

### In

- `SettingsManager`：全局 `~/.pir/agent/settings.json` + 项目覆盖；**全键清单与默认值**（需求 §7.7，40+ 键）；合并语义（嵌套对象单层浅合并（深度≥2 整体替换）、**数组与原始值整体替换**）；字段级写持久化（只写 session 内改过的字段）+ `fs2` 锁；旧格式迁移 4 条（queueMode→steeringMode、websockets→transport、旧 skills 对象、retry.maxDelayMs→provider.maxRetryDelayMs）；trust=false 时项目 settings 视为空且拒写；parse 错误按 scope 诊断并阻止覆写
- 环境变量模块（需求 §3.3）：进程级 `PIR_*` 全集 + bash 会话注入 5 变量（与 T06 接线）
- `ResourceLoader` 统一发现：全局 → 项目（trust 门控）→ settings 路径 → CLI flags → packages，输出 `LoadedResources { extensions, skills, prompts, themes, context_files, diagnostics }`（设计文档 §6.7）；同名冲突先到先得 + collision 诊断；**资源优先级 rank**（project settings > project auto > user settings > user auto > package）
- Context files：候选优先级 `AGENTS.md` > `AGENTS.MD` > `CLAUDE.md` > `CLAUDE.MD`；全局一份 + cwd 到**文件系统根**祖先链（不以 git root 为界）；`<project_context>`/`<project_instructions path>` 注入格式；**无论 trust 与否都加载**
- `SYSTEM.md` / `APPEND_SYSTEM.md`：项目版需 trust 且优先于全局；`--system-prompt`/`--append-system-prompt` 文件路径 vs 内联文本解析
- Skills：**发现路径全集**（`~/.pir/agent/skills`、`~/.agents/skills`、`.pir/skills`、祖先 `.agents/skills` 上界 **git repo root**、packages、settings 数组、CLI）；**两种发现模式**（pi 目录根级散 `.md` 算 skill；`.agents` 只认 SKILL.md）；ignore 文件链（`.gitignore`/`.ignore`/`.fdignore`）、dotdir/node_modules 跳过、符号链接跟随；frontmatter（name 校验仅警告、description 缺失**不加载**、disable-model-invocation）；渐进披露（`<available_skills>` XML，**仅 read 工具激活时注入**）；`/skill:name` 展开格式（`<skill name location>` + References 行 + args 原样追加）；`enableSkillCommands`
- Prompt templates：**非递归** `*.md`；frontmatter（description 缺省首行截 60+`...`、argument-hint）；**展开 DSL 全集**（`$1..$N`、`$@`、`$ARGUMENTS`、`${N:-default}`、`${@:-default}`、`${ARGUMENTS:-default}`、`${@:N}`、`${@:N:L}`；引号感知解析；不递归；缺位空串）
- Themes：内置 dark/light；JSON schema（`name`、`vars`、`colors` **51 必填 + `thinkingMax` 可选回退 thinkingXhigh**、`export` 段，theme-schema.json:38-90）；ColorValue 三形态（hex / 0-255 整数 / `""`）；热重载**仅 watch 全局当前主题文件**；theme 值 `light/dark` 为 auto 配对（`parseAutoThemeSetting`）、主题名正则 `^[^/]+$`；终端配色检测链 OSC 11→COLORFGBG→fallback、动态切换；终端自省 OSC 11 / CSI ?996n / CSI 16t / OSC 9;4（1s keepalive）
- Keybindings：**仅全局** `keybindings.json`；命名空间 id（`tui.editor.*`/`tui.input.*`/`tui.select.*`/`app.*`）；**旧键名迁移表 60+ 项**（ADR-0003 §3 保留项）；平台差异默认值（win32 无 ctrl+z、贴图 alt+v、macOS tree 方向键）

### Out

- 主题热重载与 TUI 应用（T12）
- Packages 安装来源加载（T14；本任务管线预留 packages 输入口）
- Project trust 两阶段加载（T14；本任务提供「信任前/后」分组能力）

## 开发要点

- 发现顺序与覆盖语义逐项对照上游文档，冲突合并规则写测试锁死
- 上游文档与代码冲突时**以代码为准**（案例：`/skill:name` 参数追加格式；设计原则 4）
- 文件格式兼容用 fixtures 验证（skills / prompts / themes / keybindings 各取上游样例）
- 新增配置路径一律走统一路径模块，不散落拼接（编码规范 §10.1）

## 进度跟踪

- [ ] 设计细化
- [ ] 实现
- [ ] 自测
- [ ] 门禁验收
- [ ] 文档回写

## 自测清单

- [ ] settings 合并语义：嵌套单层浅合并（深度≥2 整体替换）、数组整体替换、字段级持久化、旧格式迁移 4 条
- [ ] 资源发现顺序、两种 skills 模式、rank 与去重规则测试（构造多级目录 fixture）
- [ ] context files 边界：到文件系统根 vs skills 到 git root（构造 repo 内外用例）
- [ ] skills：XML 摘要注入（含 read 工具未激活时不注入）、frontmatter 各字段语义、`/skill:name` 展开格式与上游代码一致
- [ ] prompt template 展开：DSL 各形态与上游一致（含切片与默认值）
- [ ] themes：51+1 token 解析、vars 引用、256 色整数、缺失 token 回退、非法值诊断
- [ ] keybindings：旧键名迁移、平台差异默认值、token 名与上游文档逐条核对
- [ ] 环境变量：进程级全集生效；bash 注入变量模型切换即时生效

## 门禁验收

通用门禁 G1–G7 全过（G5 重点：settings/themes/keybindings JSON 形状兼容）。

任务特有标准：

- [ ] 需求 §7.1–§7.5、§7.7、§3.3 各条目有测试锚点（验收记录列映射表）
- [ ] 上游资源样例 fixtures 加载结果对拍一致

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| — | （暂无） | — |

## 验收记录

（待填写，模板见 `gates.md` §3）
