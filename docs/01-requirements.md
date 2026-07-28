# Pir 需求规格说明书（1:1 对齐 Pi v0.82.1）

> 本文档定义 `pir` 必须达到的功能与行为。除非标注 **[DEFER]** 或 **[VARIANT]**，均要求与 Pi 行为一致。  
> 对照源：`external/pi/packages/{ai,agent,tui,coding-agent}`

---

## 1. 产品目标

### 1.1 愿景

用 Rust 实现与 Pi 同构的 **minimal terminal coding agent harness**：默认精简工具集，通过扩展/技能/模板/主题/包进行扩展，支持交互、脚本、RPC 与库嵌入。

### 1.2 成功标准

1. **行为对拍**：同一配置下，print/json/rpc 模式对同一 prompt 的工具调用序列、session JSONL 结构、事件类型与 Pi 一致（允许时间戳/随机 ID 差异）。  
2. **会话互通**：能加载并续跑 Pi 生成的 session JSONL（v1–v3 自动迁移）。  
3. **资源互通**：Skills / Prompt Templates / Themes / Settings / Keybindings / `models.json` 文件格式兼容；Auth 凭据存储（credential store）JSON 结构与 Pi 兼容（仅路径在 `~/.pir` 下，可手动拷贝迁移登录态）。  
4. **架构同构**：四层 crate 边界与 Pi 四包对应。  
5. **扩展**：ExtensionAPI **形状同构**，实现为 **Rust / Wasm**（见 [ADR-0001](./adr/0001-extension-and-config-dir.md)）。**不要求**兼容现有 TypeScript / jiti pi-package 扩展。

### 1.3 非目标（与 Pi 一致）

- 不内置子 agent / plan mode / MCP（由扩展提供）
- 不内置细粒度 OS 权限沙箱（文档化 containerization 模式即可）
- 不以「更好的 UX 创新」替代 1:1（创新放后续版本）

### 1.4 命名与路径（已决策）

| 项 | Pi | Pir（默认） |
|----|-----|-------------|
| CLI | `pi` | `pir` |
| 全局配置 | `~/.pi/agent` | **`~/.pir/agent`** |
| 项目配置 | `.pi` | **`.pir`** |
| 环境变量前缀 | `PI_*` | **`PIR_*`** |

子目录布局镜像 Pi（`sessions/`、`settings.json`、`extensions/`、`skills/` 等），仅根目录名不同。详见 [ADR-0001](./adr/0001-extension-and-config-dir.md)。

不默认读写 `~/.pi` / `.pi`，**不提供**路径迁移工具。Session **文件格式**与钉死版 Pi JSONL 对齐。对照版本见 [`UPSTREAM.md`](../UPSTREAM.md) / [ADR-0002](./adr/0002-baseline-decisions.md)。

### 1.5 「1:1」的边界（术语定义）

本文档与姊妹文档中的「1:1」按以下三层理解；单独出现时默认指第 1 层：

1. **行为 1:1（对拍保证）**：事件序、session JSONL 格式与 v1–v3 迁移、KnownApi 协议适配、compaction 与 token 估算、CLI 标志与 slash 命令、声明式资源格式（Skills / Prompts / Themes / Settings / Keybindings / `models.json` / 凭据存储）、TUI 行为与渲染、RPC 帧语义。以 fixtures 对拍与黄金文件验收（见 §11）。
2. **API 形状同构（实现重写）**：扩展系统。ExtensionAPI 的形状、事件语义与能力面对齐，但扩展须以 Rust/Wasm 重写；**不兼容**现有 TS / jiti pi-package（[ADR-0001](./adr/0001-extension-and-config-dir.md)）。
3. **有意差异（ADR 钉死）**：CLI 名 `pir`、配置根 `~/.pir`、环境变量前缀 `PIR_*`（§1.4）；扩展包格式（Wasm 包替代 npm pi-package）；Rust SDK 替代 Node SDK；仅 JSONL 存储；无 `~/.pi` 迁移工具（ADR-0001 / [ADR-0002](./adr/0002-baseline-decisions.md)）。

约束：新增行为级偏差只能通过新 ADR 进入第 3 层；不允许以「Rust 实现差异」为由放宽第 1 层。

---

## 2. 运行模式

### 2.1 Interactive（默认，TTY）

- 差分渲染 TUI：header、消息区、editor、footer
- Slash 命令、快捷键、消息队列（steering / follow-up）
- Project trust 交互提示
- 扩展 UI：dialog / widget / overlay / 自定义 editor

### 2.2 Print（`-p` / 非 TTY 自动）

- 处理初始 prompt（含 piped stdin）后退出
- 打印最终助手文本（非 JSON）
- 无 trust 提示；遵循 `defaultProjectTrust` / `--approve` / `--no-approve`

### 2.3 JSON（`--mode json`）

- stdout 输出 session header + `AgentSessionEvent` JSONL
- 单向事件流，无命令环

### 2.4 RPC（`--mode rpc`）

- stdin/stdout **严格 LF** JSONL（不得按 Unicode 行分隔符拆分）
- 命令 + `type:"response"` + 异步 events
- 扩展 UI 对话框走协议往返；`ui.custom()` 不可用
- 命令面与 Pi `docs/rpc.md` 对齐（prompt/steer/follow_up/abort/session/model/…）

### 2.5 SDK

- Rust crate API 对应 `createAgentSession` / `createAgentSessionRuntime` / `SessionManager` / `ModelRuntime`
- 跨语言嵌入优先走 RPC（与 Pi 给非 Node 宿主的建议一致）

---

## 3. CLI 需求

### 3.1 主命令标志（对齐 `cli/args.ts`）

必须支持：

| 标志 | 行为 |
|------|------|
| `--provider` / `--model` / `--api-key` | 选择模型与密钥 |
| `--system-prompt` / `--append-system-prompt` | 覆盖/追加系统提示 |
| `--mode text\|json\|rpc` | 输出模式 |
| `-p` / `--print` | 非交互 |
| `-c` / `--continue` | 继续最近会话 |
| `-r` / `--resume` | 选择历史会话 |
| `--session` / `--session-id` / `--fork` / `--session-dir` / `--no-session` | 会话控制 |
| `-n` / `--name` | 会话显示名 |
| `--models` | Ctrl+P 循环模型列表 |
| `-nt` / `-nbt` / `-t` / `-xt` | 工具开关与名单 |
| `--thinking` | thinking 级别 |
| `-e` / `-ne` | 扩展路径 / 禁用发现 |
| `--skill` / `-ns` | skills |
| `--prompt-template` / `-np` | prompt templates |
| `--theme` / `--no-themes` | themes |
| `-nc` / `--no-context-files` | 禁用 AGENTS.md 等 |
| `--export` | 导出 HTML 后退出 |
| `--list-models` | 列出模型 |
| `--approve` / `-a`，`--no-approve` / `-na` | 本次运行信任 / 忽略项目本地文件（project trust） |
| `--verbose` / `--offline` / `-h` / `-v` | 杂项 |

### 3.2 子命令

| 命令 | 行为 |
|------|------|
| `pir install` | npm/git/本地包安装（`-l` 项目级） |
| `pir remove` / `uninstall` | 卸载包 |
| `pir list` | 列出已装包 |
| `pir update` | `--self` / `--all` / `--extensions` / `--models` / 单包 |
| `pir config` | 启用/禁用 extensions、skills、prompts、themes |

### 3.3 平台文档级需求

Windows / Termux / tmux / terminal-setup / shell-aliases：行为与文档说明对齐（可作为验收 checklist）。

---

## 4. Agent 运行时需求

### 4.1 消息模型

支持 `AgentMessage` 联合类型：

- 基础：`user` / `assistant` / `toolResult`（含 Text/Image/Thinking/ToolCall content blocks）
- 扩展：`bashExecution` / `custom` / `branchSummary` / `compactionSummary`

流式边界：`AssistantMessage.stopReason` ∈ `stop|length|toolUse|error|aborted`；`pending` 仅瞬时，不得写入 JSONL。

### 4.2 事件模型

Agent 事件至少包括：

`agent_start` / `agent_end` / `turn_start` / `turn_end` /  
`message_start` / `message_update` / `message_end` /  
`tool_execution_start` / `tool_execution_update` / `tool_execution_end`

事件序必须与 Pi README 中的 prompt/tool 序列一致。

### 4.3 循环语义

1. `transform_context`（可选）→ `convert_to_llm`（必选）→ `stream_fn`
2. 工具执行：`parallel`（默认）或 `sequential`；单工具可强制 sequential；parallel 下 completion 事件按完成序，持久化 toolResult **按 assistant 源序**
3. `before_tool_call` 可 block；`after_tool_call` 可改结果
4. 全部 tool result `terminate: true` 时可跳过后续 LLM
5. steering：当前 turn 工具结束后、下次 LLM 前注入
6. follow-up：agent 空闲后注入
7. `steeringMode` / `followUpMode`：`one-at-a-time` | `all`
8. abort / continue（重试）语义对齐
9. `Agent` 订阅者 await 屏障：assistant `message_end` 之后才开始 tool preflight

### 4.4 内置工具

**默认启用**：`read`、`write`、`edit`、`bash`  
**可选**：`grep`、`find`、`ls`（与 coding-agent tools 一致）

行为要求：

| 工具 | 关键行为锚点 |
|------|--------------|
| read | 文本/图像；行范围；截断策略 |
| write | 创建/覆盖；目录创建 |
| edit | 精确替换 / diff 算法（`edit-diff`）；文件 mutation queue |
| bash | 流式输出；截断；取消；`!`/`!!` 用户 bash 路径；session 环境变量注入 |
| grep/find/ls | gitignore 感知、截断、路径规则 |

工具可通过 allowlist/denylist/`--no-tools`/`--no-builtin-tools` 控制；扩展工具可同名覆盖内置。

---

## 5. LLM / Provider 需求

### 5.1 KnownApi（必须实现）

- `openai-completions`
- `openai-responses`
- `azure-openai-responses`
- `openai-codex-responses`
- `anthropic-messages`
- `google-generative-ai`
- `google-vertex`
- `bedrock-converse-stream`
- `mistral-conversations`
- `pi-messages`

### 5.2 Providers（必须具备等价能力）

对齐 Pi README 列表：Anthropic、OpenAI、Azure、Codex、DeepSeek、Google/Vertex、Bedrock、Mistral、Groq、Cerebras、xAI、OpenRouter、Vercel AI Gateway、Cloudflare、GitHub Copilot、ZAI、MiniMax、Kimi、Hugging Face、Fireworks、Together、OpenCode、Xiaomi MiMo、NVIDIA NIM、Ant Ling、llama.cpp router、任意 OpenAI-compatible 等。

### 5.3 横切能力

- Auth 解析：环境变量 → credential store → OAuth
- `/login` `/logout` 订阅流
- Thinking 统一级别：`off|minimal|low|medium|high|xhigh|max` + `thinkingBudgets`
- Image input；image generation（OpenRouter images 等）
- Token / cost / cache 统计
- Cross-provider handoff
- `models.json` 自定义模型；远程 catalog 刷新（`pir update --models`）
- Transport 偏好：`sse|websocket|websocket-cached|auto`（设置项）
- Faux provider（测试）

---

## 6. Session 需求

### 6.1 存储

- 第一版 **仅 JSONL**（不做 SQLite）
- 路径：`~/.pir/agent/sessions/--<cwd>--/<timestamp>_<uuid>.jsonl`
- 可覆盖：`--session-dir` / `PIR_CODING_AGENT_SESSION_DIR` / `settings.sessionDir`
- 版本迁移：v1 → v2 → v3（文件格式，非 Pi 用户目录迁移）
- `--no-session` 内存会话
- **不做** `~/.pi` → `~/.pir` 迁移工具

### 6.2 条目类型

`message` / `model_change` / `thinking_level_change` / `compaction` / `branch_summary` / `custom` / `label` 等与 `session-format.md` 一致。`compaction` 条目须兼容两种形态：含 `firstKeptEntryId` 的旧形态与内嵌 `retainedTail` 的新形态。

### 6.3 操作

- 继续 / 恢复 / 按 id 打开
- `/tree` 原地分支导航 + 可选 branch summarization
- `/fork`、`/clone`、CLI `--fork`
- `/new`、import/export JSONL、HTML export、gist share
- `/name`、label/bookmark

### 6.4 Compaction

- Token 估算与钉死版 Pi **完全一致**（同一算法与常量，禁止文档化偏差）
- 触发：`contextTokens > contextWindow - reserveTokens`；overflow 恢复重试；手动 `/compact [instructions]`
- 参数：`compaction.enabled` / `reserveTokens`(16384) / `keepRecentTokens`(20000)
- 切点、split turn、迭代 summary、`firstKeptEntryId`、`tokensBefore` 重算
- 扩展可自定义 compaction（`session_before_compact` 等）
- 压缩请求使用独立 routing session id，并在支持处关闭 prompt-cache write

### 6.5 与 Pi 会话互通的降级策略

加载 Pi 生成的 session（含 TS 扩展产物）时：

- **保留**：所有 entry（含 `custom`、`label`、未知类型）原样保留在 session 树中，写回时不丢数据。
- **跳过 LLM context**：无对应扩展的 `custom` message/entry 不进入 `convert_to_llm` 输出；`bashExecution` 按钉死版 Pi 的 `convertToLlm` 规则处理。
- **通用渲染**：TUI 中未知 custom entry 以通用 JSON 折叠块渲染（类型名 + 数据摘要），不报错、不阻断会话。

---

## 7. 资源与定制

### 7.1 Context files

加载 `AGENTS.md` / `CLAUDE.md`：全局 + 祖先链 + cwd；`-nc` 禁用。  
`SYSTEM.md` / `APPEND_SYSTEM.md` 覆盖/追加系统提示。

### 7.2 Skills

- Agent Skills 标准（Pi 宽松规则：name 可≠目录名）
- 发现路径与 Pi `skills.md` 一致（含 `~/.agents/skills`、祖先 `.agents/skills`）
- 渐进披露：system prompt 仅 XML 摘要；全文 on-demand
- `/skill:name [args]`；`enableSkillCommands`
- `disable-model-invocation` 等 frontmatter

### 7.3 Prompt Templates

- `*.md` → `/name`；`$1`/`$@`/`${1:-default}` 等展开规则对齐

### 7.4 Themes

- 内置 dark/light；自定义 JSON（51 color tokens）
- 热重载活跃主题文件

### 7.5 Keybindings

- `keybindings.json`；`/hotkeys`；默认表对齐 `docs/keybindings.md`

### 7.6 Packages

- `npm:` / `git:` / URL / 本地；全局与 `-l` 项目级
- `package.json#pi` manifest
- pinned ref 不被 update 升级
- `npmCommand` wrapper 支持

### 7.7 Settings

完整对齐 `docs/settings.md`（model、UI、compaction、retry、transport、packages、telemetry 等）。  
全局 `~/.…/settings.json` + 项目覆盖。

### 7.8 Project Trust

- `trust.json`；交互询问；`defaultProjectTrust`：`ask|always|never`
- **两阶段加载**：信任前仅 context + 全局/CLI 扩展；信任后加载项目 settings/扩展/包
- `/trust`；非交互模式不提示

---

## 8. Interactive UX 需求

### 8.1 布局

Startup header → messages → editor → footer（cwd、session名、tokens/cache/cost/context、model）。

### 8.2 Editor

多行、undo、kill-ring、bracketed paste（大粘贴 marker）、`@` 文件模糊搜索、Tab 路径补全、Shift+Enter 换行、Ctrl+G 外置编辑器、Ctrl+V 图文粘贴、`!`/`!!` bash。

### 8.3 消息队列

- Enter → steering  
- Alt+Enter → follow-up  
- Escape abort 并恢复队列；Alt+Up 取回队列

### 8.4 内置 Slash 命令

至少：`/login` `/logout` `/llama` `/model` `/scoped-models` `/settings` `/resume` `/new` `/name` `/session` `/tree` `/trust` `/fork` `/clone` `/compact` `/copy` `/export` `/import` `/share` `/reload` `/hotkeys` `/changelog` `/quit`

### 8.5 常用快捷键

Ctrl+C / 双 Ctrl+C、Escape / 双 Escape、Ctrl+L、Ctrl+P、Shift+Tab、Ctrl+O、Ctrl+T、Ctrl+X 等与文档一致。

### 8.6 TUI 引擎

- 三策略差分渲染 + CSI 2026
- Overlay、Focus、IME 光标
- Markdown（流式 fence 稳定）、SelectList、SettingsList、Image（Kitty/iTerm2）
- Kitty keyboard + legacy；终端特例（Windows Terminal、tmux、Apple）

---

## 9. 扩展系统需求

### 9.1 能力清单（API 形状 1:1）

- 加载发现路径与 `/reload`
- 事件：`project_trust`、`resources_discover`、session_*、agent_*、tool_*、provider hooks、`input`、`user_bash` 等（见 `extensions.md`）
- `registerTool` / `Command` / `Shortcut` / `Flag` / `Provider`
- `registerMessageRenderer` / `EntryRenderer`
- UI：`select`/`confirm`/`input`/`editor`/`notify`/`setStatus`/`setWidget`/`setHeader`/`setFooter`/`custom`/…
- `sendMessage` / `appendEntry` / 动态工具 / `setActiveTools`
- 模式差异：tui 全能力；rpc 对话框协议化；print/json UI no-op

### 9.2 实现范围（已决策）

| 级别 | 需求 ID | 状态 |
|------|---------|------|
| L0 | EXT-RUST | **必做**：Rust trait 同构 API，内置扩展 + 动态库插件 |
| L1 | EXT-WASM | **必做**：Wasm 插件 + 与 L0 同一能力面的 host ABI |
| L2 | EXT-TS | **不做**：嵌入 JS / 跑现有 `.ts` 扩展 |

扩展需用 Rust/Wasm 重写。**安装与包管理列入正式计划**（本地路径 + 可分发 Wasm 包；`install`/`remove`/`list`/`update`/`config`）。Skills / prompts / themes 声明式资源格式仍与 Pi 对齐。Wasm runtime **嵌入主二进制**。见 ADR-0001 / ADR-0002。

---

## 10. 安全、运维与分发

- 扩展/包/skills 以用户权限运行；文档警告对齐
- Containerization 文档三种模式可移植说明
- **单文件部署**：发布单一可执行文件；Wasm 扩展 runtime 打进主包
- 版本检查 / telemetry：**支持配置自有 endpoint**（settings / `PIR_*`）；可关闭
- `/debug` 写调试日志（TUI 行 + 最后发给 LLM 的消息）

## 10.1 许可证

**MIT**（与 Pi 相同）。

---

## 11. 质量需求

### 11.1 测试

- 单元：agent loop 事件序、工具序、session 迁移、compaction 切点
- 契约：RPC JSON schema / 黄金 JSONL
- Provider：faux + 可选集成测试（有 key 时）
- TUI：
  - 关键 ANSI 序列子集 diff（去 CSI 2026 同步输出抖动后，与 Pi 虚拟终端输出比对）
  - 组件级渲染快照黄金文件（Editor / SelectList / Markdown / SettingsList 等）
  - 真机矩阵（Kitty / Ghostty / Windows Terminal / tmux 等）仅 smoke 验收
- 回归：移植 Pi coding-agent regression 用例意图

### 11.2 性能

- 流式首 token 延迟相对 Node 版劣化 ≤ 20%（同网络、同 provider、各 3 次取中位数）
- TUI 渲染节流 ~16ms；大 session 滚动可用
- 二进制体积：< 50MB 压缩前（含嵌入的 wasmtime）；M0 ABI spike 时实测验证

### 11.3 兼容性

- Linux（主）、macOS、Windows
- 终端：Kitty、Ghostty、Alacritty、iTerm2、Windows Terminal、tmux、VS Code terminal

---

## 12. 文档需求

- 用户文档结构镜像 `coding-agent/docs/`
- 开发者：crate 边界、扩展 ABI、对拍测试说明
- CHANGELOG 跟踪相对上游的差异

---

## 13. 里程碑验收（需求视角）

| 里程碑（对齐设计文档 M0–M8） | 必须满足的需求章节 |
|--------|-------------------|
| M0 骨架与对拍基建 | §11.1（对拍 harness、fixtures、归一化 diff）；Wasm ABI spike |
| M1–M2 Headless 核心 | §4、§5（≥2 协议）、默认四工具 |
| M3–M4 Headless MVP | §6（含 §6.5）、§2.2–2.4、token 估算对拍 |
| M5 Interactive | §8、§3 主命令 |
| M6 Providers 全量 | §5 全量 |
| M7 资源与包管理 | §7（含 packages / trust / export / llama.cpp） |
| M8 扩展 + Parity Freeze | §9（L0+L1 与安装）、全文档对拍清单、session 互通 |

---

## 14. 需求追溯

| 需求域 | Pi 文档 / 源码 |
|--------|----------------|
| 总览 | `packages/coding-agent/README.md` |
| Session | `docs/session-format.md`, `src/core/session-manager.ts` |
| Compaction | `docs/compaction.md` |
| Extensions | `docs/extensions.md` |
| RPC/SDK/JSON | `docs/rpc.md`, `sdk.md`, `json.md` |
| Settings/Keys | `docs/settings.md`, `keybindings.md` |
| Skills/… | `docs/skills.md`, `prompt-templates.md`, `themes.md`, `packages.md` |
| Agent loop | `packages/agent/README.md`, `agent-loop.ts` |
| AI | `packages/ai/README.md` |
| TUI | `packages/tui/README.md`, `docs/tui.md` |
